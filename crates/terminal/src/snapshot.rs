//! Provider-independent terminal state tracking for durable PTY recovery.

use serde::{Deserialize, Serialize};

#[cfg(feature = "libghostty")]
use crate::ghostty_screen::GhosttyScreen;
use crate::links::{screen_links, AnchorLog, Hyperlink, ScreenLink};
use crate::program_status::{ProbeSpan, PROGRAM_STATUS_COMMAND};

// Rust guideline compliant 2026-07-23

/// Maximum OSC value retained in memory.
///
/// Titles and progress summaries are short UI metadata. Bounding the parser
/// prevents an unterminated sequence from retaining arbitrary PTY output.
const MAX_OSC_BYTES: usize = 4 * 1024;

/// DEC private mode number of synchronized output.
const SYNCHRONIZED_OUTPUT_MODE: u32 = 2026;

/// Begins a synchronized-output bracket (DEC private mode 2026).
const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
/// Ends a synchronized-output bracket (DEC private mode 2026).
const SYNC_END: &[u8] = b"\x1b[?2026l";
/// Clear the screen and home the cursor: the prologue both backends' repaints
/// carry and the degraded error snapshot carries (issue #2538 M13).
const REPAINT_CLEAR: &[u8] = b"\x1b[2J\x1b[H";

/// Reading the screen state of the active backend failed.
///
/// The vt100 backend cannot fail; this arises only on the libghostty
/// backend, whose reads and formatting go through the C library and can
/// return any `GhosttyResult` code, not only `OUT_OF_MEMORY` (issue #2593
/// finding 8). [`TerminalTracker::try_snapshot`] reports it;
/// [`TerminalTracker::snapshot`] degrades to an error snapshot instead of
/// panicking under the worker's output lock.
#[derive(Debug)]
pub struct SnapshotError {
    /// The library error; only the libghostty backend can produce one.
    #[cfg(feature = "libghostty")]
    ghostty: pohunek_terminal_ghostty_sys::GhosttyError,
}

impl SnapshotError {
    /// Wraps a libghostty read or format failure.
    #[cfg(feature = "libghostty")]
    pub(crate) fn libghostty(error: pohunek_terminal_ghostty_sys::GhosttyError) -> Self {
        Self { ghostty: error }
    }
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        #[cfg(feature = "libghostty")]
        {
            write!(f, "libghostty screen read failed: {}", self.ghostty)
        }
        #[cfg(not(feature = "libghostty"))]
        {
            write!(f, "the screen state read failed")
        }
    }
}

impl std::error::Error for SnapshotError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        #[cfg(feature = "libghostty")]
        {
            Some(&self.ghostty)
        }
        #[cfg(not(feature = "libghostty"))]
        {
            None
        }
    }
}

/// Identifier of the screen model that produced a [`TerminalSnapshot`].
///
/// Serialized as a `snake_case` string so a wire reader can name the backend
/// that rendered the snapshot. A new model is a new variant; an older reader
/// that does not know it fails the decode of that one field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenModelId {
    /// The pure-Rust `vt100` parser, the model Pohunek ships today.
    #[default]
    Vt100,
    /// libghostty-vt through `pohunek-terminal-ghostty-sys`, available only
    /// with the `libghostty` feature.
    Libghostty,
}

impl From<ModelKind> for ScreenModelId {
    fn from(model: ModelKind) -> Self {
        match model {
            ModelKind::Vt100 => Self::Vt100,
            #[cfg(feature = "libghostty")]
            ModelKind::Libghostty => Self::Libghostty,
        }
    }
}

/// Wire format version of the snapshot payload this build emits.
///
/// Bump this whenever the meaning of a snapshot field changes, a parser pin
/// moves, or a repaint option changes, so a reader of the bytes can tell which
/// pin produced them. A snapshot without the field decodes as version 1.
pub const CURRENT_FORMAT_VERSION: u32 = 1;

/// serde default of [`TerminalSnapshot::format_version`].
fn current_format_version() -> u32 {
    CURRENT_FORMAT_VERSION
}

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
    /// Screen model that produced this snapshot.
    #[serde(default)]
    pub model: ScreenModelId,
    /// Wire format version of the snapshot payload.
    #[serde(default = "current_format_version")]
    pub format_version: u32,
    /// Current OSC title, when observed.
    pub title: Option<String>,
    /// Current OSC progress payload, when observed.
    pub progress: Option<String>,
    /// Visible text of the active screen rows only, without terminal styling.
    ///
    /// Scrolled-off history is never included on either backend, so a long
    /// scrollback cannot grow the snapshot past the screen (issue #2538 M2);
    /// the libghostty backend takes this view from the sys crate's
    /// `HeadlessTerminal::visible_text`, the vt100 backend from
    /// `Screen::contents`.
    pub visible_text: String,
    /// ANSI stream that recreates the complete visible state.
    pub ansi: Vec<u8>,
}

/// The screen model a [`TerminalTracker`] runs.
///
/// Non-exhaustive so a later release can add models without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ModelKind {
    /// The pure-Rust `vt100` parser, the model Pohunek ships today.
    Vt100,
    /// libghostty-vt through `pohunek-terminal-ghostty-sys`, available only
    /// with the `libghostty` feature.
    #[cfg(feature = "libghostty")]
    Libghostty,
}

/// The screen backend of a tracker: one VT parser per model. The `OscTracker`
/// is shared and byte-driven, so OSC metadata does not depend on the backend.
///
/// The vt100 parser is boxed: it is by far the largest variant, and the tracker
/// is held in one allocation per session either way.
enum ScreenBackend {
    Vt100(Box<vt100::Parser>),
    #[cfg(feature = "libghostty")]
    Ghostty(GhosttyScreen),
}

/// What `TerminalTracker::snapshot` fills a [`TerminalSnapshot`] from.
pub(crate) struct ScreenState {
    pub rows: u16,
    pub cols: u16,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cursor_visible: bool,
    pub alternate_screen: bool,
    pub visible_text: String,
    /// The backend's own repaint body, without the synchronized-output
    /// bracket the tracker adds around it.
    pub repaint: Vec<u8>,
}

impl ScreenBackend {
    fn feed(&mut self, bytes: &[u8]) {
        match self {
            Self::Vt100(parser) => parser.process(bytes),
            #[cfg(feature = "libghostty")]
            Self::Ghostty(screen) => screen.feed(bytes),
        }
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        match self {
            Self::Vt100(parser) => parser.screen_mut().set_size(rows, cols),
            #[cfg(feature = "libghostty")]
            Self::Ghostty(screen) => screen.resize(rows, cols),
        }
    }

    fn size(&self) -> (u16, u16) {
        match self {
            Self::Vt100(parser) => parser.screen().size(),
            #[cfg(feature = "libghostty")]
            Self::Ghostty(screen) => screen.size(),
        }
    }

    /// Applies a completed OSC 8 sequence to the shared anchor record, at the
    /// cursor the backend has after the bytes fed so far.
    fn apply_hyperlink(&mut self, anchors: &mut AnchorLog, hyperlink: Hyperlink) {
        match self {
            Self::Vt100(parser) => anchors.apply(parser.screen(), hyperlink),
            #[cfg(feature = "libghostty")]
            Self::Ghostty(screen) => anchors.apply(&screen.anchor_view(), hyperlink),
        }
    }

    // Fallible only with the libghostty backend; the vt100 arm always succeeds.
    #[cfg_attr(
        not(feature = "libghostty"),
        allow(clippy::unnecessary_wraps, reason = "the vt100-only build cannot fail")
    )]
    fn state(&self) -> Result<ScreenState, SnapshotError> {
        match self {
            Self::Vt100(parser) => {
                let screen = parser.screen();
                let (rows, cols) = screen.size();
                let (cursor_row, cursor_col) = screen.cursor_position();
                let alternate_screen = screen.alternate_screen();
                let mut repaint = Vec::new();
                if alternate_screen {
                    repaint.extend_from_slice(b"\x1b[?1049h");
                }
                repaint.extend_from_slice(REPAINT_CLEAR);
                repaint.extend_from_slice(&screen.input_mode_formatted());
                repaint.extend_from_slice(&screen.contents_formatted());
                repaint.extend_from_slice(&screen.cursor_state_formatted());
                Ok(ScreenState {
                    rows,
                    cols,
                    cursor_row,
                    cursor_col,
                    cursor_visible: !screen.hide_cursor(),
                    alternate_screen,
                    visible_text: screen.contents(),
                    repaint,
                })
            }
            // The libghostty backend can fail; the caller degrades to an
            // error snapshot instead of panicking (issue #2593 finding 8).
            #[cfg(feature = "libghostty")]
            Self::Ghostty(screen) => screen.state().map_err(SnapshotError::libghostty),
        }
    }
}

/// Tracks one provider-independent VT screen and bounded OSC metadata.
pub struct TerminalTracker {
    backend: ScreenBackend,
    osc: OscTracker,
    anchors: AnchorLog,
    /// The current size, updated on every resize: the degraded error snapshot
    /// reports it, because on the error path a failed backend cannot be asked
    /// (issue #2593 finding 8).
    size: std::cell::Cell<(u16, u16)>,
}

impl std::fmt::Debug for TerminalTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalTracker")
            .field("size", &self.backend.size())
            .field("osc", &self.osc)
            .field("anchors", &self.anchors)
            .field("tracked_size", &self.size.get())
            .finish()
    }
}

impl TerminalTracker {
    /// Creates an empty tracker with the initial terminal dimensions, running
    /// the default model of this build: libghostty when the `libghostty`
    /// feature is on, vt100 otherwise.
    #[must_use]
    pub fn new(rows: u16, cols: u16) -> Self {
        #[cfg(feature = "libghostty")]
        let model = ModelKind::Libghostty;
        #[cfg(not(feature = "libghostty"))]
        let model = ModelKind::Vt100;
        Self::with_model(model, rows, cols)
    }

    /// Creates an empty tracker that runs `model` explicitly, so one build can
    /// drive or compare both screen models.
    #[must_use]
    pub fn with_model(model: ModelKind, rows: u16, cols: u16) -> Self {
        let backend = match model {
            ModelKind::Vt100 => ScreenBackend::Vt100(Box::new(vt100::Parser::new(rows, cols, 0))),
            #[cfg(feature = "libghostty")]
            ModelKind::Libghostty => ScreenBackend::Ghostty(GhosttyScreen::new(rows, cols)),
        };
        Self {
            backend,
            osc: OscTracker::new(),
            anchors: AnchorLog::default(),
            size: std::cell::Cell::new((rows, cols)),
        }
    }

    /// The screen model this tracker runs.
    ///
    /// Public alongside [`with_model`](Self::with_model) and accepted by the
    /// plan: `snapshot` fills `TerminalSnapshot::model` from it, and tests
    /// assert the default of a build with it (issue #2593 finding 10).
    #[must_use]
    pub fn model(&self) -> ModelKind {
        match &self.backend {
            ScreenBackend::Vt100(_) => ModelKind::Vt100,
            #[cfg(feature = "libghostty")]
            ScreenBackend::Ghostty(_) => ModelKind::Libghostty,
        }
    }

    /// Applies the next ordered PTY output bytes.
    ///
    /// The screen backend is advanced up to the end of each OSC 8 sequence and
    /// of each buffer-switch sequence before it is applied, so an anchor
    /// records the cursor cell the hyperlink takes effect at, and a buffer
    /// switch drops the anchors wherever the output is split into chunks. The
    /// anchor record is shared and byte-driven on both backends, like the OSC
    /// metadata.
    pub fn feed(&mut self, bytes: &[u8]) {
        let mut rest = bytes;
        while !rest.is_empty() {
            let (used, event) = self.osc.feed_until_event(rest);
            self.backend.feed(&rest[..used]);
            rest = &rest[used..];
            match event {
                Some(TrackerEvent::Hyperlink(hyperlink)) => {
                    self.backend.apply_hyperlink(&mut self.anchors, hyperlink);
                }
                Some(TrackerEvent::BufferSwitch) => self.anchors.clear(),
                None => {}
            }
        }
    }

    /// Returns the links on the visible screen, in reading order, none with a
    /// URL longer than `max_url_bytes`.
    ///
    /// OSC 8 anchors are reported while the text they covered is still in the
    /// cells where it was printed; a scroll, clear, overwrite, resize or
    /// buffer switch drops them, on both backends (the anchor record is
    /// byte-driven and shared). Nothing is restored when the buffer is
    /// switched back.
    #[must_use]
    pub fn links(&self, max_url_bytes: usize) -> Vec<ScreenLink> {
        match &self.backend {
            ScreenBackend::Vt100(parser) => {
                screen_links(parser.screen(), &self.anchors, max_url_bytes)
            }
            #[cfg(feature = "libghostty")]
            ScreenBackend::Ghostty(screen) => screen.links(&self.anchors, max_url_bytes),
        }
    }

    /// Takes the OSC 7501 probes completed since the previous call.
    ///
    /// Spans are offsets in the stream fed to this tracker, in order.
    pub fn take_probes(&mut self) -> Vec<ProbeSpan> {
        std::mem::take(&mut self.osc.probes)
    }

    /// Updates the modeled terminal dimensions.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.backend.resize(rows, cols);
        self.anchors.clear();
        // The degraded error snapshot reports this size: on the error path a
        // failed backend cannot be asked, so this is the freshest size known
        // without a fallible read. A rejected ghostty resize leaves the
        // library at its old size, so this is the requested size there.
        self.size.set((rows, cols));
    }

    /// Captures a complete state and ANSI repaint at `watermark`.
    ///
    /// Never panics: a backend read failure degrades to
    /// [`degraded_snapshot`](Self::degraded_snapshot) with the failure
    /// logged. This runs under the worker's output mutex, where a panic would
    /// poison it (issue #2593 finding 8).
    #[must_use]
    pub fn snapshot(&self, watermark: u64) -> TerminalSnapshot {
        match self.try_snapshot(watermark) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "the screen state read failed; degrading to an error snapshot"
                );
                self.degraded_snapshot(watermark)
            }
        }
    }

    /// Captures a complete state and ANSI repaint at `watermark`, or reports
    /// the backend read failure instead of degrading.
    ///
    /// The worker calls this one so a degraded snapshot is never cached: on
    /// `Err` it serves [`degraded_snapshot`](Self::degraded_snapshot) without
    /// storing it, and the next read retries the backend (issue #2593
    /// finding 8).
    pub fn try_snapshot(&self, watermark: u64) -> Result<TerminalSnapshot, SnapshotError> {
        let state = self.backend.state()?;
        let mut ansi = Vec::new();
        // One bracket spans the whole repaint, so a terminal that implements
        // mode 2026 draws it as one frame however the bytes are chunked.
        ansi.extend_from_slice(SYNC_BEGIN);
        ansi.extend_from_slice(&state.repaint);
        // A program that set mode 2026 keeps it: the bracket's begin already
        // left it set, and a reset here would end the program's own frame.
        if !self.osc.synchronized_output {
            ansi.extend_from_slice(SYNC_END);
        }

        Ok(TerminalSnapshot {
            watermark,
            rows: state.rows,
            cols: state.cols,
            cursor_row: state.cursor_row,
            cursor_col: state.cursor_col,
            cursor_visible: state.cursor_visible,
            alternate_screen: state.alternate_screen,
            model: self.model().into(),
            format_version: CURRENT_FORMAT_VERSION,
            title: self.osc.title.clone(),
            progress: self.osc.progress.clone(),
            visible_text: state.visible_text,
            ansi,
        })
    }

    /// The error snapshot of a failed backend read: the current size, empty
    /// text, and the same `CSI 2J CSI H` clear the repaints carry, so an
    /// attach on it still wipes the client screen (issue #2538 M13). The
    /// caller must not cache it — the next read retries the backend.
    ///
    /// Not reachable through the safe API today: every read `state()` makes
    /// is in bounds, so only an allocation- or I/O-class failure takes the
    /// error path. Proving the degradation needs a fault-injection hook in
    /// the sys crate (a test-only handle whose reads fail), which would drive
    /// `try_snapshot` to `Err` and assert the clear, the fresh size and the
    /// absent cache (issue #2593 finding 8; the row stays open until then).
    #[must_use]
    pub fn degraded_snapshot(&self, watermark: u64) -> TerminalSnapshot {
        let (rows, cols) = self.size.get();
        let mut ansi = Vec::new();
        ansi.extend_from_slice(SYNC_BEGIN);
        ansi.extend_from_slice(REPAINT_CLEAR);
        if !self.osc.synchronized_output {
            ansi.extend_from_slice(SYNC_END);
        }
        TerminalSnapshot {
            watermark,
            rows,
            cols,
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            alternate_screen: false,
            model: self.model().into(),
            format_version: CURRENT_FORMAT_VERSION,
            title: self.osc.title.clone(),
            progress: self.osc.progress.clone(),
            visible_text: String::new(),
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
    /// Index of the latest `ESC` that may begin an escape sequence: one seen in
    /// the ground state or one that interrupts a CSI or another `ESC`.
    escape_at: u64,
    /// Index of the `ESC` that introduced the current OSC.
    osc_start: u64,
    probes: Vec<ProbeSpan>,
    /// Event completed by the byte just advanced, not yet taken.
    event: Option<TrackerEvent>,
    /// Value of the private-mode CSI parameter being read.
    csi_param: u32,
    /// Whether an earlier parameter of that CSI names an alternate screen.
    csi_selects: bool,
    /// Whether an earlier parameter of that CSI names mode 2026.
    csi_sync: bool,
    /// Whether the program itself set DEC private mode 2026 and has not reset it.
    synchronized_output: bool,
}

/// Something the tracker applies between two parser runs.
#[derive(Debug)]
enum TrackerEvent {
    /// An OSC 8 sequence completed.
    Hyperlink(Hyperlink),
    /// A set or reset of an alternate-screen mode (`?47`, `?1047`, `?1049`).
    BufferSwitch,
}

/// Whether a private mode number selects an alternate screen buffer.
fn is_alternate_screen_mode(mode: u32) -> bool {
    matches!(mode, 47 | 1047 | 1049)
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
            event: None,
            csi_param: 0,
            csi_selects: false,
            csi_sync: false,
            synchronized_output: false,
        }
    }

    /// Advances over `bytes` up to and including the byte that completes an
    /// OSC 8 sequence or a buffer switch, or over all of them when none does.
    ///
    /// Returns the number of bytes consumed and the completed event.
    fn feed_until_event(&mut self, bytes: &[u8]) -> (usize, Option<TrackerEvent>) {
        for (index, &byte) in bytes.iter().enumerate() {
            self.advance(byte);
            self.consumed += 1;
            if let Some(event) = self.event.take() {
                return (index + 1, Some(event));
            }
        }
        (bytes.len(), None)
    }

    /// Reads one byte of a private-mode CSI the way the VT parser does: a
    /// decimal parameter that saturates, and C0 controls inside the sequence do
    /// not end it.
    fn advance_private_mode(&mut self, byte: u8) {
        match byte {
            b'0'..=b'9' => {
                self.csi_param = self
                    .csi_param
                    .saturating_mul(10)
                    .saturating_add(u32::from(byte - b'0'));
            }
            b';' => {
                self.csi_selects |= is_alternate_screen_mode(self.csi_param);
                self.csi_sync |= self.csi_param == SYNCHRONIZED_OUTPUT_MODE;
                self.csi_param = 0;
            }
            b'h' | b'l' => {
                if self.csi_selects || is_alternate_screen_mode(self.csi_param) {
                    self.event = Some(TrackerEvent::BufferSwitch);
                }
                if self.csi_sync || self.csi_param == SYNCHRONIZED_OUTPUT_MODE {
                    self.synchronized_output = byte == b'h';
                }
                self.state = OscState::Ground;
            }
            0x1b => {
                self.escape_at = self.consumed;
                self.state = OscState::Escape;
            }
            0x18 | 0x1a => self.state = OscState::Ground,
            // DEL is ignored and the other C0 controls execute without
            // ending the sequence, as a VT parser treats both CSI states.
            0x00..=0x1f | 0x7f => {}
            _ => self.state = OscState::Ground,
        }
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
                self.state = match byte {
                    b']' => {
                        self.osc_start = self.escape_at;
                        self.command.clear();
                        self.payload.clear();
                        OscState::Command
                    }
                    b'[' => OscState::Csi,
                    0x1b => {
                        self.escape_at = self.consumed;
                        OscState::Escape
                    }
                    b'c' => {
                        // RIS resets every mode, 2026 included.
                        self.synchronized_output = false;
                        OscState::Ground
                    }
                    _ => OscState::Ground,
                };
            }
            OscState::Csi => {
                self.state = match byte {
                    b'?' => {
                        self.csi_param = 0;
                        self.csi_selects = false;
                        self.csi_sync = false;
                        OscState::CsiPrivate
                    }
                    0x1b => {
                        self.escape_at = self.consumed;
                        OscState::Escape
                    }
                    0x18 | 0x1a => OscState::Ground,
                    // A VT parser ignores DEL in CSI entry and executes the
                    // other C0 controls there without ending the sequence.
                    0x00..=0x1f | 0x7f => OscState::Csi,
                    _ => OscState::Ground,
                };
            }
            OscState::CsiPrivate => self.advance_private_mode(byte),
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
            Some("8") => {
                self.event = Hyperlink::parse(&self.payload).map(TrackerEvent::Hyperlink);
            }
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
    /// After `ESC [`.
    Csi,
    /// After `ESC [ ?`, reading private-mode parameters.
    CsiPrivate,
    Command,
    Payload,
    PayloadEscape,
    Discard,
    DiscardEscape,
}

#[cfg(test)]
mod tests {
    use super::{ModelKind, TerminalTracker, SYNC_END};

    /// Whether the program's own mode 2026 is tracked as set: the repaint
    /// appends its closing reset only while the mode is unset.
    fn mode_2026_is_set(tracker: &TerminalTracker) -> bool {
        !tracker.snapshot(0).ansi.ends_with(SYNC_END)
    }

    #[test]
    fn fragmented_vt_and_osc_sequences_produce_complete_snapshot() {
        // The vt100 model is selected explicitly: with the `libghostty`
        // feature `new` runs the libghostty backend, whose repaint bytes
        // differ.
        let mut tracker = TerminalTracker::with_model(ModelKind::Vt100, 3, 20);
        tracker.feed(b"\x1b]2;build");
        tracker.feed(b"ing\x07\x1b[2;4Hok");

        let snapshot = tracker.snapshot(42);

        assert_eq!(snapshot.watermark, 42);
        assert_eq!(snapshot.title.as_deref(), Some("building"));
        assert!(snapshot.visible_text.contains("ok"));
        assert_eq!((snapshot.cursor_row, snapshot.cursor_col), (1, 5));
        assert!(snapshot.ansi.starts_with(b"\x1b[?2026h\x1b[2J\x1b[H"));
    }

    #[test]
    fn oversized_osc_is_discarded_and_parser_recovers() {
        let mut tracker = TerminalTracker::with_model(ModelKind::Vt100, 2, 10);
        let oversized = vec![b'x'; super::MAX_OSC_BYTES + 1];
        tracker.feed(b"\x1b]2;");
        tracker.feed(&oversized);
        tracker.feed(b"\x07\x1b]2;safe\x07");

        assert_eq!(tracker.snapshot(0).title.as_deref(), Some("safe"));
    }

    #[test]
    fn del_and_c0_in_csi_entry_do_not_end_the_sequence() {
        // A VT parser ignores DEL in CSI entry and executes the other C0
        // controls there without ending the sequence, so `ESC [ DEL ? 2026 h`
        // and `ESC [ NUL ? 2026 h` set mode 2026 on a real terminal and here.
        for prefix in [&b"\x1b[\x7f"[..], &b"\x1b[\x00"[..], &b"\x1b[\x1b["[..]] {
            let mut tracker = TerminalTracker::with_model(ModelKind::Vt100, 2, 10);
            let mut bytes = prefix.to_vec();
            bytes.extend_from_slice(b"?2026h");
            tracker.feed(&bytes);
            assert!(
                mode_2026_is_set(&tracker),
                "{prefix:?} must not end the CSI: the private mode still applies"
            );
        }
    }

    #[test]
    fn can_and_sub_in_csi_entry_abort_the_sequence() {
        // CAN and SUB end any escape sequence, so the `? 2026 h` after them is
        // printed text and mode 2026 stays unset. The abort byte must arrive
        // while the tracker is in the `Csi` state (after `ESC [`), not in
        // `Escape`.
        for abort in [0x18_u8, 0x1a] {
            let mut tracker = TerminalTracker::with_model(ModelKind::Vt100, 2, 10);
            tracker.feed(&[0x1b, b'[', abort]);
            tracker.feed(b"?2026h");
            assert!(
                !mode_2026_is_set(&tracker),
                "byte {abort:#x} must abort the CSI before the private marker"
            );
        }
    }
}
