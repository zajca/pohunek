//! Composites a transient modal above a live agent screen.
//!
//! [`Compositor`] shadows the attached PTY byte stream in a `vt100` grid while
//! the client normally forwards bytes unchanged. When a modal opens, it freezes
//! the current grid as a background, saves the physical cursor state, and draws
//! an overlay dialog. The caller buffers agent bytes until
//! [`Compositor::restore`] repaints the frozen background; replaying those bytes
//! then returns the physical terminal to the exact agent-authored state.
//!
//! # Rendering model
//!
//! - The shadow grid uses the full physical terminal size, preserving raw
//!   passthrough geometry and native scrollback outside the modal.
//! - Physical rows `1..=N` are available to the centered overlay; session
//!   details travel in the overlay's own header rows.
//! - Modal drawing never changes the scroll region or alternate-screen mode.

// Rust guideline compliant 2026-10-03

use std::fmt;

/// Minimum physical rows of the shadow grid.
///
/// One row keeps the vt100 grid addressable and leaves at least one row for
/// modal content.
pub const MIN_ROWS: u16 = 1;

/// No scrollback: the compositor mirrors the visible screen like a raw attach.
const SCROLLBACK_LINES: usize = 0;

// Escape sequences authored by the compositor while the modal owns drawing.
const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
const ORIGIN_MODE_OFF: &[u8] = b"\x1b[?6l";
const RESET_ATTRS: &[u8] = b"\x1b[m";
const CLEAR_LINE: &[u8] = b"\x1b[2K";
/// DEC save/restore preserves the agent cursor, attributes, and origin mode.
const SAVE_CURSOR: &[u8] = b"\x1b7";
const RESTORE_CURSOR: &[u8] = b"\x1b8";

/// The first physical terminal column in one-based cursor coordinates.
const FIRST_PHYSICAL_COL: u16 = 1;
/// The first physical terminal row in one-based cursor coordinates.
const FIRST_PHYSICAL_ROW: u16 = 1;

/// One-cell padding on both sides keeps overlay text away from the border.
const OVERLAY_HORIZONTAL_PADDING_COLUMNS: u16 = 2;
/// One left padding cell before overlay text.
const OVERLAY_LEFT_PADDING_COLUMNS: u16 = 1;
/// One right padding cell after overlay text.
const OVERLAY_RIGHT_PADDING_COLUMNS: u16 = 1;
/// Two border columns: one left edge and one right edge.
const OVERLAY_HORIZONTAL_BORDER_COLUMNS: u16 = 2;
/// Two border rows: one top edge and one bottom edge.
const OVERLAY_VERTICAL_BORDER_ROWS: u16 = 2;
/// The title consumes one interior row before content lines.
const OVERLAY_TITLE_ROWS: u16 = 1;
/// A footer, when present, consumes one interior row after content lines.
const OVERLAY_FOOTER_ROWS: u16 = 1;
/// A border-only box needs at least both vertical border cells.
const OVERLAY_MIN_WIDTH: u16 = OVERLAY_HORIZONTAL_BORDER_COLUMNS;
/// The top border is the first row of the box.
const OVERLAY_TOP_BORDER_OFFSET: u16 = 0;
/// The title is the first row inside the border.
const OVERLAY_TITLE_OFFSET: u16 = 0;
/// Interior coordinates start after the top border row.
const OVERLAY_TOP_BORDER_ROWS: u16 = 1;
/// Interior coordinates start after the left border column.
const OVERLAY_LEFT_BORDER_COLUMNS: u16 = 1;
/// ASCII border glyphs avoid encoding ambiguity in terminal tests and logs.
const OVERLAY_TOP_LEFT: u8 = b'+';
const OVERLAY_TOP_RIGHT: u8 = b'+';
const OVERLAY_BOTTOM_LEFT: u8 = b'+';
const OVERLAY_BOTTOM_RIGHT: u8 = b'+';
const OVERLAY_HORIZONTAL_BORDER: u8 = b'-';
const OVERLAY_VERTICAL_BORDER: u8 = b'|';
const OVERLAY_FILL: u8 = b' ';
/// Reverse video marks the title as the active overlay heading.
const OVERLAY_TITLE_ATTRS: &[u8] = b"\x1b[7m";
/// Reverse video marks the currently selected overlay content row.
const OVERLAY_HIGHLIGHT_ATTRS: &[u8] = b"\x1b[7m";

/// Describes a small overlay box for composited attach screens.
///
/// The frame carries plain text and selection state. [`Compositor`] owns all
/// terminal geometry, clipping, borders, and style bytes so callers cannot draw
/// outside the modal content region.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct OverlayFrame {
    /// Heading rendered in the first interior row.
    pub title: String,
    /// Plain detail rows rendered between the title and the body lines.
    pub header: Vec<String>,
    /// Body lines rendered below the title.
    pub lines: Vec<OverlayLine>,
    /// Optional footer rendered after body lines when space permits.
    pub footer: Option<String>,
    /// Zero-based cursor cell: the row is relative to the first body line and
    /// the column to the box interior.
    ///
    /// `None` hides the physical cursor while the overlay is visible.
    pub cursor: Option<(u16, u16)>,
}

/// Describes one body line inside an [`OverlayFrame`].
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct OverlayLine {
    /// Plain text rendered inside the overlay body.
    pub text: String,
    /// Whether this line is rendered with the overlay highlight style.
    pub highlighted: bool,
}

/// Composites a transient menu above a shadowed agent screen.
///
/// Feed raw PTY bytes with [`Compositor::feed`], then obtain physical-terminal
/// bytes with [`Compositor::render`]. Call [`Compositor::resize`] on window
/// changes and [`Compositor::reset`] when detaching to restore the terminal.
pub struct Compositor {
    parser: vt100::Parser,
    /// Frozen physical screen from the moment the modal opened.
    background: Option<vt100::Screen>,
    overlay: Option<OverlayFrame>,
    cols: u16,
    rows: u16,
}

impl fmt::Debug for Compositor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Compositor")
            .field("cols", &self.cols)
            .field("rows", &self.rows)
            .finish_non_exhaustive()
    }
}

impl Compositor {
    /// Creates a compositor for a physical terminal of `cols` by `rows`.
    ///
    /// The shadow grid uses full passthrough geometry. `rows` is clamped to at
    /// least [`MIN_ROWS`].
    #[must_use]
    pub fn new(cols: u16, rows: u16) -> Self {
        let rows = rows.max(MIN_ROWS);
        Self {
            parser: vt100::Parser::new(rows, cols, SCROLLBACK_LINES),
            background: None,
            overlay: None,
            cols,
            rows,
        }
    }

    /// Feeds raw agent PTY bytes into the internal grid without drawing.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// Sets the optional overlay drawn above the live agent grid.
    pub fn set_overlay(&mut self, overlay: Option<OverlayFrame>) {
        self.overlay = overlay;
    }

    /// Replaces the header rows of the current overlay; a no-op without one.
    pub fn set_overlay_header(&mut self, header: Vec<String>) {
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.header = header;
        }
    }

    /// Resizes the shadow grid while no modal is open.
    ///
    /// # Panics
    ///
    /// Panics when called while a modal background is active.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        assert!(
            self.background.is_none(),
            "cannot resize the compositor while a modal is active"
        );
        let rows = rows.max(MIN_ROWS);
        self.cols = cols;
        self.rows = rows;
        self.parser.screen_mut().set_size(rows, cols);
    }

    /// The agent grid size `(rows, cols)` the daemon PTY should be sized to.
    #[must_use]
    pub fn grid_size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    /// Renders physical-terminal bytes that update the screen to the fed state.
    ///
    /// The first call freezes the current shadow grid and saves the physical
    /// cursor state. Later calls repaint that same background before drawing the
    /// latest overlay, clearing stale modal geometry.
    #[must_use]
    pub fn render(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.background.is_none() {
            self.background = Some(self.parser.screen().clone());
            out.extend_from_slice(SAVE_CURSOR);
            out.extend_from_slice(ORIGIN_MODE_OFF);
        }
        out.extend_from_slice(HIDE_CURSOR);
        self.write_background(&mut out);
        if let Some(overlay) = self.overlay.as_ref() {
            self.write_overlay(&mut out, overlay);
        }
        self.write_cursor(&mut out);
        out
    }

    /// Restores the frozen screen before buffered agent bytes are replayed.
    ///
    /// Returns no bytes when no modal is active.
    #[must_use]
    pub fn restore(&mut self) -> Vec<u8> {
        let Some(background) = self.background.take() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        write_full_grid(&mut out, &background, self.cols);
        out.extend_from_slice(RESET_ATTRS);
        out.extend_from_slice(RESTORE_CURSOR);
        if background.hide_cursor() {
            out.extend_from_slice(HIDE_CURSOR);
        } else {
            out.extend_from_slice(SHOW_CURSOR);
        }
        self.overlay = None;
        out
    }

    fn write_background(&self, out: &mut Vec<u8>) {
        let background = self
            .background
            .as_ref()
            .expect("render initializes a modal background");
        write_full_grid(out, background, self.cols);
    }

    fn write_overlay(&self, out: &mut Vec<u8>, overlay: &OverlayFrame) {
        let Some(geometry) = self.overlay_geometry(overlay) else {
            return;
        };

        for row_offset in OVERLAY_TOP_BORDER_OFFSET..geometry.height {
            let row = geometry.row.saturating_add(row_offset);
            push_move(out, row, geometry.col);
            write_overlay_row(out, overlay, geometry, row_offset);
        }
    }

    fn overlay_geometry(&self, overlay: &OverlayFrame) -> Option<OverlayGeometry> {
        if self.cols == 0 {
            return None;
        }

        let grid_height = self.rows.max(1);
        let max_content_width = overlay_content_width(overlay);
        let desired_width = max_content_width
            .saturating_add(OVERLAY_HORIZONTAL_BORDER_COLUMNS)
            .saturating_add(OVERLAY_HORIZONTAL_PADDING_COLUMNS);
        let width = desired_width.max(OVERLAY_MIN_WIDTH).min(self.cols);

        let footer_rows = overlay.footer.as_ref().map_or(0, |_| OVERLAY_FOOTER_ROWS);
        let desired_height = OVERLAY_VERTICAL_BORDER_ROWS
            .saturating_add(OVERLAY_TITLE_ROWS)
            .saturating_add(len_u16(&overlay.header))
            .saturating_add(len_u16(&overlay.lines))
            .saturating_add(footer_rows);
        let height = desired_height.min(grid_height);

        let row = FIRST_PHYSICAL_ROW.saturating_add((grid_height.saturating_sub(height)) / 2);
        let col = FIRST_PHYSICAL_COL.saturating_add((self.cols.saturating_sub(width)) / 2);

        Some(OverlayGeometry {
            row,
            col,
            width,
            height,
        })
    }

    fn write_cursor(&self, out: &mut Vec<u8>) {
        if let Some(overlay) = self.overlay.as_ref() {
            self.write_overlay_cursor(out, overlay);
            return;
        }

        let screen = self.parser.screen();
        let (row, col) = screen.cursor_position();
        let grid_row = grid_row_number(usize::from(row));
        push_move(out, grid_row, col.saturating_add(1));
        if screen.hide_cursor() {
            out.extend_from_slice(HIDE_CURSOR);
        } else {
            out.extend_from_slice(SHOW_CURSOR);
        }
    }

    fn write_overlay_cursor(&self, out: &mut Vec<u8>, overlay: &OverlayFrame) {
        let Some((row, col)) = overlay.cursor else {
            out.extend_from_slice(HIDE_CURSOR);
            return;
        };

        let Some(geometry) = self.overlay_geometry(overlay) else {
            out.extend_from_slice(HIDE_CURSOR);
            return;
        };

        let Some((row, col)) = overlay_cursor_position(geometry, overlay, row, col) else {
            out.extend_from_slice(HIDE_CURSOR);
            return;
        };

        push_move(out, row, col);
        out.extend_from_slice(SHOW_CURSOR);
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct OverlayGeometry {
    row: u16,
    col: u16,
    width: u16,
    height: u16,
}

/// One-based grid row for the zero-based `vt100` visible-row `index`.
///
fn grid_row_number(index: usize) -> u16 {
    let index = u16::try_from(index).expect("grid row index fits in u16");
    index.saturating_add(FIRST_PHYSICAL_ROW)
}

fn write_full_grid(out: &mut Vec<u8>, screen: &vt100::Screen, cols: u16) {
    for (index, row) in screen.rows_formatted(0, cols).enumerate() {
        push_move(out, grid_row_number(index), FIRST_PHYSICAL_COL);
        out.extend_from_slice(RESET_ATTRS);
        out.extend_from_slice(CLEAR_LINE);
        out.extend_from_slice(&row);
    }
}

fn push_move(out: &mut Vec<u8>, row: u16, col: u16) {
    out.extend_from_slice(format!("\x1b[{row};{col}H").as_bytes());
}

fn overlay_content_width(overlay: &OverlayFrame) -> u16 {
    let title_width = cell_width(&overlay.title);
    let header_width = overlay
        .header
        .iter()
        .map(|text| cell_width(text))
        .max()
        .unwrap_or(0);
    let line_width = overlay
        .lines
        .iter()
        .map(|line| cell_width(&line.text))
        .max()
        .unwrap_or(0);
    let footer_width = overlay.footer.as_deref().map_or(0, cell_width);

    title_width
        .max(header_width)
        .max(line_width)
        .max(footer_width)
}

fn write_overlay_row(
    out: &mut Vec<u8>,
    overlay: &OverlayFrame,
    geometry: OverlayGeometry,
    row_offset: u16,
) {
    if row_offset == OVERLAY_TOP_BORDER_OFFSET {
        write_overlay_border_row(out, geometry.width, OVERLAY_TOP_LEFT, OVERLAY_TOP_RIGHT);
        return;
    }

    if row_offset == geometry.height.saturating_sub(1) {
        write_overlay_border_row(
            out,
            geometry.width,
            OVERLAY_BOTTOM_LEFT,
            OVERLAY_BOTTOM_RIGHT,
        );
        return;
    }

    let interior_offset = row_offset.saturating_sub(OVERLAY_TOP_BORDER_ROWS);
    if interior_offset == OVERLAY_TITLE_OFFSET {
        write_overlay_text_row(
            out,
            geometry.width,
            &overlay.title,
            Some(OVERLAY_TITLE_ATTRS),
        );
        return;
    }

    let after_title = interior_offset.saturating_sub(OVERLAY_TITLE_ROWS);
    if let Some(text) = overlay.header.get(usize::from(after_title)) {
        write_overlay_text_row(out, geometry.width, text, None);
        return;
    }

    let line_offset = after_title.saturating_sub(len_u16(&overlay.header));
    if let Some(line) = overlay.lines.get(usize::from(line_offset)) {
        let attrs = line.highlighted.then_some(OVERLAY_HIGHLIGHT_ATTRS);
        write_overlay_text_row(out, geometry.width, &line.text, attrs);
        return;
    }

    let footer_offset = line_offset.saturating_sub(len_u16(&overlay.lines));
    if footer_offset == 0 {
        if let Some(footer) = overlay.footer.as_ref() {
            write_overlay_text_row(out, geometry.width, footer, None);
            return;
        }
    }

    write_overlay_text_row(out, geometry.width, "", None);
}

fn cell_width(text: &str) -> u16 {
    u16::try_from(text.chars().count()).unwrap_or(u16::MAX)
}

fn len_u16<T>(items: &[T]) -> u16 {
    u16::try_from(items.len()).unwrap_or(u16::MAX)
}

fn write_overlay_border_row(out: &mut Vec<u8>, width: u16, left: u8, right: u8) {
    if width == 0 {
        return;
    }

    out.push(left);
    if width > 1 {
        write_fill(
            out,
            OVERLAY_HORIZONTAL_BORDER,
            width.saturating_sub(OVERLAY_HORIZONTAL_BORDER_COLUMNS),
        );
        out.push(right);
    }
    out.extend_from_slice(RESET_ATTRS);
}

fn write_overlay_text_row(out: &mut Vec<u8>, width: u16, text: &str, attrs: Option<&'static [u8]>) {
    if width == 0 {
        return;
    }

    out.push(OVERLAY_VERTICAL_BORDER);
    if width == 1 {
        out.extend_from_slice(RESET_ATTRS);
        return;
    }

    let interior_width = width.saturating_sub(OVERLAY_HORIZONTAL_BORDER_COLUMNS);
    let left_padding = OVERLAY_LEFT_PADDING_COLUMNS.min(interior_width);
    write_fill(out, OVERLAY_FILL, left_padding);

    let after_left_padding = interior_width.saturating_sub(left_padding);
    let right_padding = OVERLAY_RIGHT_PADDING_COLUMNS.min(after_left_padding);
    let text_width = after_left_padding.saturating_sub(right_padding);

    if let Some(attrs) = attrs {
        out.extend_from_slice(attrs);
    }
    let written = write_clipped_text(out, text, text_width);
    if attrs.is_some() {
        out.extend_from_slice(RESET_ATTRS);
    }

    write_fill(out, OVERLAY_FILL, text_width.saturating_sub(written));
    write_fill(out, OVERLAY_FILL, right_padding);
    out.push(OVERLAY_VERTICAL_BORDER);
    out.extend_from_slice(RESET_ATTRS);
}

fn write_clipped_text(out: &mut Vec<u8>, text: &str, width: u16) -> u16 {
    let mut written = 0;
    for ch in text.chars().take(usize::from(width)) {
        let mut buf = [0; 4];
        out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        written += 1;
    }
    written
}

fn write_fill(out: &mut Vec<u8>, byte: u8, width: u16) {
    out.extend(std::iter::repeat_n(byte, usize::from(width)));
}

fn overlay_cursor_position(
    geometry: OverlayGeometry,
    overlay: &OverlayFrame,
    row: u16,
    col: u16,
) -> Option<(u16, u16)> {
    let interior_height = geometry.height.saturating_sub(OVERLAY_VERTICAL_BORDER_ROWS);
    let interior_width = geometry
        .width
        .saturating_sub(OVERLAY_HORIZONTAL_BORDER_COLUMNS);
    if interior_height == 0 || interior_width == 0 {
        return None;
    }

    let row = row
        .saturating_add(OVERLAY_TITLE_ROWS)
        .saturating_add(len_u16(&overlay.header))
        .min(interior_height.saturating_sub(1));
    let col = col.min(interior_width.saturating_sub(1));
    Some((
        geometry
            .row
            .saturating_add(OVERLAY_TOP_BORDER_ROWS)
            .saturating_add(row),
        geometry
            .col
            .saturating_add(OVERLAY_LEFT_BORDER_COLUMNS)
            .saturating_add(col),
    ))
}

#[cfg(test)]
mod tests {
    use super::{Compositor, OverlayFrame, OverlayLine, MIN_ROWS};

    const SHORT_TEST_COLS: u16 = 20;
    const SHORT_TEST_ROWS: u16 = 8;
    const SHORT_OVERLAY_LEFT_COL: u16 = 4;
    const SHORT_OVERLAY_TOP_ROW: u16 = 2;
    const CLIPPED_TEST_COLS: u16 = 10;
    const CLIPPED_TEST_ROWS: u16 = 4;
    const CLIPPED_OVERLAY_BOTTOM_ROW: u16 = CLIPPED_TEST_ROWS;
    const CLIPPED_OVERLAY_AFTER_BOTTOM_ROW: u16 = CLIPPED_TEST_ROWS + 1;
    const CURSOR_TEST_ROW: u16 = 4;
    const CURSOR_TEST_COL: u16 = 7;

    fn render_string(compositor: &mut Compositor) -> String {
        String::from_utf8(compositor.render()).expect("frame is utf8")
    }

    fn move_to(row: u16, col: u16) -> String {
        format!("\x1b[{row};{col}H")
    }

    fn test_overlay() -> OverlayFrame {
        OverlayFrame {
            title: "Menu".to_owned(),
            header: Vec::new(),
            lines: vec![
                OverlayLine {
                    text: "Kill".to_owned(),
                    highlighted: true,
                },
                OverlayLine {
                    text: "Detach".to_owned(),
                    highlighted: false,
                },
            ],
            footer: Some("Esc closes".to_owned()),
            cursor: None,
        }
    }

    #[test]
    fn grid_size_matches_raw_passthrough_geometry() {
        let compositor = Compositor::new(80, 24);

        assert_eq!(compositor.grid_size(), (24, 80));
    }

    #[test]
    fn rows_are_clamped_to_leave_an_addressable_grid() {
        let compositor = Compositor::new(80, 0);

        assert_eq!(compositor.grid_size(), (MIN_ROWS, 80));
    }

    #[test]
    fn first_frame_saves_terminal_state_and_keeps_row_one_for_the_agent() {
        let mut compositor = Compositor::new(40, 10);
        compositor.feed(b"hello");

        let frame = render_string(&mut compositor);

        assert!(
            frame.starts_with("\x1b7\x1b[?6l"),
            "first frame must save the cursor before using absolute coordinates: {frame:?}"
        );
        assert!(
            !frame.contains("\x1b[7m\x1b[2K"),
            "no banner row may be drawn over the agent grid: {frame:?}"
        );
        assert!(
            !frame.contains("\x1b[2;10r") && !frame.contains("\x1b[?1049"),
            "modal drawing must not change scroll margins or screen buffers: {frame:?}"
        );
        assert!(
            frame.contains("\x1b[1;1H") && frame.contains("hello"),
            "frozen agent output must be available behind the modal: {frame:?}"
        );
    }

    #[test]
    fn alternate_screen_state_is_shadowed_without_reemitting_switches() {
        let mut compositor = Compositor::new(40, 10);
        // A full-screen TUI: enter the alternate screen, home the cursor, draw.
        compositor.feed(b"\x1b[?1049h\x1b[2J\x1b[1;1HTUI");

        let frame = render_string(&mut compositor);

        // The raw alternate-screen switch must never reach the physical
        // terminal; it was absorbed into the grid.
        assert!(
            !frame.contains("\x1b[?1049h"),
            "alternate-screen switch must be swallowed, not forwarded: {frame:?}"
        );
        assert!(
            frame.contains("TUI"),
            "alternate-screen content must be available in the frozen background: {frame:?}"
        );
        assert!(
            !frame.contains("\x1b[2;10r"),
            "transient modal drawing must leave scroll margins untouched: {frame:?}"
        );
    }

    #[test]
    fn modal_does_not_reemit_agent_input_modes() {
        let mut compositor = Compositor::new(40, 10);
        // Application cursor keys + bracketed paste, as a TUI would enable.
        compositor.feed(b"\x1b[?1h\x1b[?2004h");

        let frame = render_string(&mut compositor);

        assert!(
            !frame.contains("\x1b[?1h"),
            "raw passthrough already owns application cursor mode: {frame:?}"
        );
        assert!(
            !frame.contains("\x1b[?2004h"),
            "raw passthrough already owns bracketed paste mode: {frame:?}"
        );
    }

    #[test]
    fn second_frame_keeps_the_frozen_background() {
        let mut compositor = Compositor::new(40, 6);
        compositor.feed(b"line one\r\nline two");
        let _ = compositor.render();

        compositor.feed(b"\r\nline two changed");
        let frame = render_string(&mut compositor);

        assert!(
            !frame.contains("\x1b[2;6r"),
            "an incremental frame must not re-establish the scroll region: {frame:?}"
        );
        assert!(
            !frame.contains("changed"),
            "agent output received during the modal must stay buffered: {frame:?}"
        );
        assert!(
            frame.contains("line one"),
            "the entry background must be repainted to clear stale overlay cells: {frame:?}"
        );
    }

    #[test]
    fn resize_uses_full_passthrough_geometry_after_restore() {
        let mut compositor = Compositor::new(40, 6);
        compositor.feed(b"content");
        let _ = compositor.render();
        let _ = compositor.restore();

        compositor.resize(50, 12);
        assert_eq!(compositor.grid_size(), (12, 50));

        let frame = render_string(&mut compositor);
        assert!(
            !frame.contains("\x1b[2;12r"),
            "modal rendering must not establish a scroll region: {frame:?}"
        );
        assert!(
            frame.contains("\x1b[1;1H"),
            "resize must repaint the full grid: {frame:?}"
        );
    }

    #[test]
    fn restore_repaints_background_and_restores_saved_cursor() {
        let mut compositor = Compositor::new(40, 8);
        compositor.feed(b"base");
        let _ = compositor.render();

        let teardown = String::from_utf8(compositor.restore()).expect("teardown is utf8");

        assert!(
            teardown.contains("base"),
            "restore must repaint the frozen background: {teardown:?}"
        );
        assert!(
            teardown.contains("\x1b8"),
            "restore must restore the saved cursor and origin mode: {teardown:?}"
        );
        assert!(
            !teardown.contains("\x1b[r") && !teardown.contains("\x1b[?1049"),
            "restore must not change scroll margins or screen buffers: {teardown:?}"
        );
        assert!(
            teardown.contains("\x1b[?25h"),
            "restore must recover the entry cursor visibility: {teardown:?}"
        );
        assert!(compositor.restore().is_empty());
    }

    #[test]
    fn hidden_cursor_is_respected_across_the_frame() {
        let mut compositor = Compositor::new(40, 6);
        compositor.feed(b"\x1b[?25l");

        let frame = render_string(&mut compositor);

        assert!(
            frame.trim_end().ends_with("\x1b[?25l"),
            "a hidden agent cursor must stay hidden after the frame: {frame:?}"
        );
    }

    #[test]
    fn overlay_draws_centered_within_the_grid() {
        let mut compositor = Compositor::new(SHORT_TEST_COLS, SHORT_TEST_ROWS);
        compositor.set_overlay(Some(test_overlay()));

        let frame = render_string(&mut compositor);

        assert!(
            frame.contains(&move_to(SHORT_OVERLAY_TOP_ROW, SHORT_OVERLAY_LEFT_COL)),
            "overlay must start centered in physical agent rows: {frame:?}"
        );
        assert!(
            frame.contains("Menu"),
            "overlay title must be rendered: {frame:?}"
        );
        assert!(
            frame.contains("Kill"),
            "overlay content must be rendered: {frame:?}"
        );
        assert!(
            frame.contains("Esc closes"),
            "overlay footer must be rendered: {frame:?}"
        );
    }

    #[test]
    fn overlay_is_clipped_to_the_agent_region() {
        let mut compositor = Compositor::new(CLIPPED_TEST_COLS, CLIPPED_TEST_ROWS);
        compositor.set_overlay(Some(OverlayFrame {
            title: "Very long overlay title".to_owned(),
            header: Vec::new(),
            lines: vec![
                OverlayLine {
                    text: "First long row".to_owned(),
                    highlighted: false,
                },
                OverlayLine {
                    text: "Second long row".to_owned(),
                    highlighted: false,
                },
            ],
            footer: Some("Long footer".to_owned()),
            cursor: None,
        }));

        let frame = render_string(&mut compositor);

        assert!(
            frame.contains(&move_to(1, 1)),
            "clipped overlay must start at the first physical row: {frame:?}"
        );
        assert!(
            frame.contains(&move_to(CLIPPED_OVERLAY_BOTTOM_ROW, 1)),
            "clipped overlay must draw through the last physical row: {frame:?}"
        );
        assert!(
            !frame.contains(&move_to(CLIPPED_OVERLAY_AFTER_BOTTOM_ROW, 1)),
            "clipped overlay must not draw past the terminal height: {frame:?}"
        );
        assert!(
            !frame.contains("Very long"),
            "overlay text must be horizontally clipped to the box width: {frame:?}"
        );
        assert!(
            !frame.contains("First long"),
            "content beyond the clipped height must not be rendered: {frame:?}"
        );
    }

    #[test]
    fn agent_output_during_modal_does_not_replace_the_overlay() {
        let mut compositor = Compositor::new(SHORT_TEST_COLS, SHORT_TEST_ROWS);
        compositor.set_overlay(Some(test_overlay()));
        let _ = compositor.render();

        compositor.feed(b"\x1b[1;1HUNDERLAY");
        let frame = render_string(&mut compositor);

        assert!(
            !frame.contains("UNDERLAY") && frame.contains("Menu"),
            "the frozen background must remain visible until buffered output is replayed: {frame:?}"
        );
    }

    #[test]
    fn opening_overlay_repaints_the_frozen_background() {
        let mut compositor = Compositor::new(SHORT_TEST_COLS, SHORT_TEST_ROWS);
        compositor.feed(b"base");
        let _ = compositor.render();

        compositor.set_overlay(Some(test_overlay()));
        let frame = render_string(&mut compositor);

        assert!(
            frame.contains("base"),
            "opening repaint must include the existing grid contents: {frame:?}"
        );
    }

    #[test]
    fn updating_open_overlay_repaints_its_content() {
        let mut compositor = Compositor::new(SHORT_TEST_COLS, SHORT_TEST_ROWS);
        compositor.set_overlay(Some(test_overlay()));
        let _ = compositor.render();

        let mut updated = test_overlay();
        updated.lines[0].highlighted = false;
        updated.lines[1].highlighted = true;
        compositor.set_overlay(Some(updated));
        let frame = render_string(&mut compositor);

        assert!(
            frame.contains("Detach"),
            "updated overlay content must still be redrawn: {frame:?}"
        );
    }

    #[test]
    fn restore_clears_overlay_with_the_frozen_background() {
        let mut compositor = Compositor::new(SHORT_TEST_COLS, SHORT_TEST_ROWS);
        compositor.feed(b"covered");
        compositor.set_overlay(Some(test_overlay()));
        let _ = compositor.render();

        compositor.set_overlay(None);
        let frame = String::from_utf8(compositor.restore()).expect("restore is utf8");

        assert!(
            frame.contains("covered"),
            "restore must repaint content hidden by the overlay: {frame:?}"
        );
    }

    #[test]
    fn overlay_hides_cursor_when_no_overlay_cursor_is_set() {
        let mut compositor = Compositor::new(SHORT_TEST_COLS, SHORT_TEST_ROWS);
        compositor.set_overlay(Some(test_overlay()));

        let frame = render_string(&mut compositor);

        assert!(
            frame.trim_end().ends_with("\x1b[?25l"),
            "overlay without a cursor must leave the physical cursor hidden: {frame:?}"
        );
    }

    #[test]
    fn overlay_cursor_is_parked_inside_the_box_and_shown() {
        let mut compositor = Compositor::new(SHORT_TEST_COLS, SHORT_TEST_ROWS);
        let mut overlay = test_overlay();
        overlay.cursor = Some((0, 2));
        compositor.set_overlay(Some(overlay));

        let frame = render_string(&mut compositor);

        assert!(
            frame.contains(&format!(
                "{}\x1b[?25h",
                move_to(CURSOR_TEST_ROW, CURSOR_TEST_COL)
            )),
            "overlay cursor must use absolute physical coordinates and be shown: {frame:?}"
        );
    }

    #[test]
    fn overlay_header_rows_render_between_title_and_body() {
        let mut compositor = Compositor::new(SHORT_TEST_COLS, SHORT_TEST_ROWS);
        let mut overlay = test_overlay();
        overlay.header = vec!["host: local".to_owned(), "branch: main".to_owned()];
        compositor.set_overlay(Some(overlay));

        let frame = render_string(&mut compositor);

        let title = frame.find("Menu").expect("title rendered");
        let host = frame
            .find("host: local")
            .expect("first header row rendered");
        let branch = frame
            .find("branch: main")
            .expect("second header row rendered");
        let body = frame.find("Kill").expect("body rendered");
        assert!(
            title < host && host < branch && branch < body,
            "header rows must sit between the title and the body: {frame:?}"
        );
    }

    #[test]
    fn overlay_header_is_clipped_to_the_terminal_width() {
        let mut compositor = Compositor::new(CLIPPED_TEST_COLS, SHORT_TEST_ROWS);
        let mut overlay = test_overlay();
        overlay.header = vec!["branch: a-very-long-branch-name".to_owned()];
        compositor.set_overlay(Some(overlay));

        let frame = render_string(&mut compositor);

        assert!(
            !frame.contains("a-very-long"),
            "header text must be horizontally clipped to the box width: {frame:?}"
        );
    }

    #[test]
    fn overlay_cursor_accounts_for_header_rows() {
        let mut with_header = Compositor::new(SHORT_TEST_COLS, SHORT_TEST_ROWS);
        let mut overlay = test_overlay();
        overlay.header = vec!["host".to_owned()];
        overlay.cursor = Some((0, 2));
        with_header.set_overlay(Some(overlay));

        let frame = render_string(&mut with_header);

        // The taller box starts one row higher (row 1) and the first body line
        // sits after the border, title and one header row.
        assert!(
            frame.contains(&format!("{}\x1b[?25h", move_to(4, CURSOR_TEST_COL))),
            "cursor row must skip the header rows: {frame:?}"
        );
    }

    #[test]
    fn set_overlay_header_updates_the_current_overlay_only() {
        let mut compositor = Compositor::new(SHORT_TEST_COLS, SHORT_TEST_ROWS);
        compositor.set_overlay_header(vec!["ignored".to_owned()]);
        compositor.set_overlay(Some(test_overlay()));
        compositor.set_overlay_header(vec!["host: local".to_owned()]);

        let frame = render_string(&mut compositor);

        assert!(frame.contains("host: local"), "{frame:?}");
        assert!(!frame.contains("ignored"), "{frame:?}");
    }
}
