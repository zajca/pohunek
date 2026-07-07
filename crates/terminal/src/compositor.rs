//! Composites a reserved banner row above a live agent screen.
//!
//! [`Compositor`] parses the attached PTY byte stream into its own `vt100` grid
//! and re-renders the physical terminal itself, exactly like a terminal
//! multiplexer. The attached agent's raw control sequences (alternate-screen
//! switches, scroll regions, absolute cursor moves) are absorbed into the grid
//! and never reach the physical terminal, so nothing competes with the banner.
//!
//! This is the crucial difference from a passthrough overlay: because the
//! compositor is the *only* writer to the physical terminal, it can hold a
//! stable scroll region (rows `2..=N`) and DEC origin mode for the lifetime of
//! the attach. A full-screen TUI such as Codex or Claude Code can no longer
//! reset those margins, because its bytes are parsed, not forwarded.
//!
//! # Rendering model
//!
//! - Physical row 1 is the banner, drawn outside the scroll region with origin
//!   mode temporarily disabled.
//! - Physical rows `2..=N` host the agent grid, sized `(rows - 1, cols)`. With
//!   origin mode enabled, the absolute cursor moves that `vt100` bakes into
//!   [`rows_formatted`](vt100::Screen::rows_formatted) land at the offset
//!   physical rows automatically.
//! - Input modes the agent enables (application cursor, bracketed paste, mouse
//!   reporting) are propagated to the physical terminal via
//!   [`input_mode_formatted`](vt100::Screen::input_mode_formatted) /
//!   [`input_mode_diff`](vt100::Screen::input_mode_diff), so keyboard, paste,
//!   and mouse continue to reach the agent.

// Rust guideline compliant 2026-07-07

use std::fmt;

/// Physical rows reserved for the banner at the top of the terminal.
///
/// The agent grid is the terminal height minus this. Changing it would require
/// widening the reserved region and re-deriving the scroll-region top row.
pub const BANNER_ROWS: u16 = 1;

/// Minimum physical rows required to host a banner plus a usable agent row.
///
/// One row for the banner and at least one for the agent viewport; below this
/// the caller should attach without a banner.
pub const MIN_ROWS_WITH_BANNER: u16 = 2;

/// No scrollback: the compositor mirrors the visible screen like a raw attach.
const SCROLLBACK_LINES: usize = 0;

// Escape sequences authored by the compositor. All physical-terminal output
// originates here, so these are the only source of scroll-region / origin-mode
// / cursor-visibility state on the real terminal.
const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
const ORIGIN_MODE_ON: &[u8] = b"\x1b[?6h";
const ORIGIN_MODE_OFF: &[u8] = b"\x1b[?6l";
const RESET_SCROLL_REGION: &[u8] = b"\x1b[r";
const RESET_ATTRS: &[u8] = b"\x1b[m";
const CLEAR_SCREEN: &[u8] = b"\x1b[2J";
const CLEAR_TO_EOL: &[u8] = b"\x1b[K";
// Reverse video for the banner, cleared to end of line so stale text never
// shows through when the banner shrinks.
const BANNER_OPEN: &[u8] = b"\x1b[7m\x1b[2K";

/// The first physical terminal column in one-based cursor coordinates.
const FIRST_PHYSICAL_COL: u16 = 1;
/// The first physical terminal row in one-based cursor coordinates.
const FIRST_PHYSICAL_ROW: u16 = 1;
/// The first physical row available for the agent grid and overlays.
const FIRST_AGENT_ROW: u16 = BANNER_ROWS + FIRST_PHYSICAL_ROW;

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
/// outside the reserved agent region.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct OverlayFrame {
    /// Heading rendered in the first interior row.
    pub title: String,
    /// Body lines rendered below the title.
    pub lines: Vec<OverlayLine>,
    /// Optional footer rendered after body lines when space permits.
    pub footer: Option<String>,
    /// Zero-based cursor cell relative to the box interior.
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

/// Composites a banner row above a live agent grid for `pohunek attach`.
///
/// Feed raw PTY bytes with [`Compositor::feed`], then obtain physical-terminal
/// bytes with [`Compositor::render`]. Call [`Compositor::resize`] on window
/// changes and [`Compositor::reset`] when detaching to restore the terminal.
pub struct Compositor {
    parser: vt100::Parser,
    overlay: Option<OverlayFrame>,
    /// Previous rendered screen, used for incremental diffs. `None` forces a
    /// full repaint (first frame and after a resize).
    prev: Option<vt100::Screen>,
    /// Previously drawn banner text; `None` forces a banner redraw.
    prev_banner: Option<String>,
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
    /// The agent grid is sized `(rows - 1, cols)`, reserving the top row for the
    /// banner. `rows` is clamped to at least [`MIN_ROWS_WITH_BANNER`].
    #[must_use]
    pub fn new(cols: u16, rows: u16) -> Self {
        let rows = rows.max(MIN_ROWS_WITH_BANNER);
        Self {
            parser: vt100::Parser::new(grid_rows(rows), cols, SCROLLBACK_LINES),
            overlay: None,
            prev: None,
            prev_banner: None,
            cols,
            rows,
        }
    }

    /// Feeds raw agent PTY bytes into the internal grid without drawing.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// Sets the optional overlay drawn above the live agent grid.
    ///
    /// Opening or closing the overlay invalidates the diff baseline so the grid
    /// under the box is repainted on the next frame. Updating an already-open
    /// overlay keeps the baseline intact for menu navigation.
    pub fn set_overlay(&mut self, overlay: Option<OverlayFrame>) {
        let presence_changed = self.overlay.is_some() != overlay.is_some();
        self.overlay = overlay;
        if presence_changed {
            self.prev = None;
        }
    }

    /// Resizes the composited grid and forces a full repaint on the next render.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let rows = rows.max(MIN_ROWS_WITH_BANNER);
        self.cols = cols;
        self.rows = rows;
        self.parser.screen_mut().set_size(grid_rows(rows), cols);
        // A new geometry invalidates the diff baseline and the reserved region.
        self.prev = None;
        self.prev_banner = None;
    }

    /// The agent grid size `(rows, cols)` the daemon PTY should be sized to.
    #[must_use]
    pub fn grid_size(&self) -> (u16, u16) {
        (grid_rows(self.rows), self.cols)
    }

    /// Returns the parsed mouse-reporting mode requested by the agent.
    #[must_use]
    pub fn mouse_protocol_mode(&self) -> vt100::MouseProtocolMode {
        self.parser.screen().mouse_protocol_mode()
    }

    /// Returns the parsed mouse coordinate encoding requested by the agent.
    #[must_use]
    pub fn mouse_protocol_encoding(&self) -> vt100::MouseProtocolEncoding {
        self.parser.screen().mouse_protocol_encoding()
    }

    /// Renders physical-terminal bytes that update the screen to the fed state.
    ///
    /// `banner` is the plain banner text; the compositor styles and clamps it to
    /// the terminal width and draws it on the reserved top row. The first call
    /// (and the first after [`Compositor::resize`]) emits a full repaint that
    /// establishes the scroll region and origin mode; later calls emit minimal
    /// diffs.
    #[must_use]
    pub fn render(&mut self, banner: &str) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(HIDE_CURSOR);

        let full = self.prev.is_none();
        if full {
            self.write_setup(&mut out);
            // `write_setup` leaves origin mode off, so the banner addresses the
            // physical top row directly.
            write_banner(&mut out, banner, self.cols);
            out.extend_from_slice(ORIGIN_MODE_ON);
            self.write_full_grid(&mut out);
        } else {
            if self.prev_banner.as_deref() != Some(banner) {
                out.extend_from_slice(ORIGIN_MODE_OFF);
                write_banner(&mut out, banner, self.cols);
                out.extend_from_slice(ORIGIN_MODE_ON);
            }
            self.write_diff_grid(&mut out);
        }

        self.write_input_modes(&mut out, full);
        if let Some(overlay) = self.overlay.as_ref() {
            self.write_overlay(&mut out, overlay);
        }
        self.write_cursor(&mut out);

        self.prev_banner = Some(banner.to_owned());
        self.prev = Some(self.parser.screen().clone());
        out
    }

    /// Restores the physical terminal after detach: full screen, no banner.
    ///
    /// Resets the scroll region and origin mode, disables any input modes the
    /// agent enabled, restores default attributes, and shows the cursor.
    #[must_use]
    pub fn reset(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        // Turn off every input mode the agent may have enabled by diffing the
        // live screen against a pristine one.
        let pristine = vt100::Parser::new(grid_rows(self.rows), self.cols, SCROLLBACK_LINES);
        out.extend_from_slice(&pristine.screen().input_mode_diff(self.parser.screen()));
        out.extend_from_slice(RESET_SCROLL_REGION);
        out.extend_from_slice(ORIGIN_MODE_OFF);
        out.extend_from_slice(RESET_ATTRS);
        out.extend_from_slice(SHOW_CURSOR);
        // Park the cursor on the last physical row so the shell prompt resumes
        // below the composited area rather than over it.
        push_move(&mut out, self.rows, 1);
        self.prev = None;
        self.prev_banner = None;
        out
    }

    fn write_setup(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(RESET_SCROLL_REGION);
        out.extend_from_slice(ORIGIN_MODE_OFF);
        out.extend_from_slice(CLEAR_SCREEN);
        // Reserve the top row: scroll region covers the agent grid only.
        out.extend_from_slice(format!("\x1b[{};{}r", BANNER_ROWS + 1, self.rows).as_bytes());
    }

    fn write_full_grid(&self, out: &mut Vec<u8>) {
        let screen = self.parser.screen();
        for (index, row) in screen.rows_formatted(0, self.cols).enumerate() {
            let grid_row = grid_row_number(index);
            // Origin mode maps grid row 1 to the first physical row below the
            // banner; reset attributes so `rows_formatted`'s default-attr
            // assumption holds, then clear the line before painting it.
            push_move(out, grid_row, 1);
            out.extend_from_slice(RESET_ATTRS);
            out.extend_from_slice(CLEAR_TO_EOL);
            out.extend_from_slice(&row);
        }
    }

    fn write_diff_grid(&self, out: &mut Vec<u8>) {
        let screen = self.parser.screen();
        let Some(prev) = self.prev.as_ref() else {
            return;
        };
        for (index, row) in screen.rows_diff(prev, 0, self.cols).enumerate() {
            if row.is_empty() {
                continue;
            }
            let grid_row = grid_row_number(index);
            // `rows_diff` assumes the cursor starts at grid (index, 0) with
            // default attributes; satisfy both before emitting the diff.
            push_move(out, grid_row, 1);
            out.extend_from_slice(RESET_ATTRS);
            out.extend_from_slice(&row);
        }
    }

    fn write_input_modes(&self, out: &mut Vec<u8>, full: bool) {
        let screen = self.parser.screen();
        match (full, self.prev.as_ref()) {
            (false, Some(prev)) => out.extend_from_slice(&screen.input_mode_diff(prev)),
            _ => out.extend_from_slice(&screen.input_mode_formatted()),
        }
    }

    fn write_overlay(&self, out: &mut Vec<u8>, overlay: &OverlayFrame) {
        let Some(geometry) = self.overlay_geometry(overlay) else {
            return;
        };

        out.extend_from_slice(ORIGIN_MODE_OFF);
        for row_offset in OVERLAY_TOP_BORDER_OFFSET..geometry.height {
            let row = geometry.row.saturating_add(row_offset);
            push_move(out, row, geometry.col);
            write_overlay_row(out, overlay, geometry, row_offset);
        }
        out.extend_from_slice(ORIGIN_MODE_ON);
    }

    fn overlay_geometry(&self, overlay: &OverlayFrame) -> Option<OverlayGeometry> {
        if self.cols == 0 {
            return None;
        }

        let grid_height = grid_rows(self.rows);
        let max_content_width = overlay_content_width(overlay);
        let desired_width = max_content_width
            .saturating_add(OVERLAY_HORIZONTAL_BORDER_COLUMNS)
            .saturating_add(OVERLAY_HORIZONTAL_PADDING_COLUMNS);
        let width = desired_width.max(OVERLAY_MIN_WIDTH).min(self.cols);

        let footer_rows = overlay.footer.as_ref().map_or(0, |_| OVERLAY_FOOTER_ROWS);
        let desired_height = OVERLAY_VERTICAL_BORDER_ROWS
            .saturating_add(OVERLAY_TITLE_ROWS)
            .saturating_add(lines_len_u16(&overlay.lines))
            .saturating_add(footer_rows);
        let height = desired_height.min(grid_height);

        let row = FIRST_AGENT_ROW.saturating_add((grid_height.saturating_sub(height)) / 2);
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

        let Some((row, col)) = overlay_cursor_position(geometry, row, col) else {
            out.extend_from_slice(HIDE_CURSOR);
            return;
        };

        out.extend_from_slice(ORIGIN_MODE_OFF);
        push_move(out, row, col);
        out.extend_from_slice(SHOW_CURSOR);
        out.extend_from_slice(ORIGIN_MODE_ON);
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct OverlayGeometry {
    row: u16,
    col: u16,
    width: u16,
    height: u16,
}

/// Agent grid height for a physical terminal of `rows` rows.
fn grid_rows(rows: u16) -> u16 {
    rows.saturating_sub(BANNER_ROWS).max(1)
}

/// One-based grid row for the zero-based `vt100` visible-row `index`.
///
/// With origin mode enabled the terminal offsets this into the reserved region,
/// so callers address the agent grid as if it were the whole screen.
fn grid_row_number(index: usize) -> u16 {
    let index = u16::try_from(index).expect("grid row index fits in u16");
    index.saturating_add(FIRST_PHYSICAL_ROW)
}

fn push_move(out: &mut Vec<u8>, row: u16, col: u16) {
    out.extend_from_slice(format!("\x1b[{row};{col}H").as_bytes());
}

/// Draws the banner text on the physical top row, clamped to `cols`.
///
/// The caller is responsible for having origin mode disabled so `\x1b[1;1H`
/// addresses the physical top row.
fn write_banner(out: &mut Vec<u8>, banner: &str, cols: u16) {
    push_move(out, FIRST_PHYSICAL_ROW, FIRST_PHYSICAL_COL);
    out.extend_from_slice(BANNER_OPEN);
    let clamped: String = banner.chars().take(usize::from(cols)).collect();
    out.extend_from_slice(clamped.as_bytes());
    out.extend_from_slice(RESET_ATTRS);
}

fn overlay_content_width(overlay: &OverlayFrame) -> u16 {
    let title_width = cell_width(&overlay.title);
    let line_width = overlay
        .lines
        .iter()
        .map(|line| cell_width(&line.text))
        .max()
        .unwrap_or(0);
    let footer_width = overlay.footer.as_deref().map_or(0, cell_width);

    title_width.max(line_width).max(footer_width)
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

    let line_offset = interior_offset.saturating_sub(OVERLAY_TITLE_ROWS);
    if let Some(line) = overlay.lines.get(usize::from(line_offset)) {
        let attrs = line.highlighted.then_some(OVERLAY_HIGHLIGHT_ATTRS);
        write_overlay_text_row(out, geometry.width, &line.text, attrs);
        return;
    }

    let footer_offset = line_offset.saturating_sub(lines_len_u16(&overlay.lines));
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

fn lines_len_u16(lines: &[OverlayLine]) -> u16 {
    u16::try_from(lines.len()).unwrap_or(u16::MAX)
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

fn overlay_cursor_position(geometry: OverlayGeometry, row: u16, col: u16) -> Option<(u16, u16)> {
    let interior_height = geometry.height.saturating_sub(OVERLAY_VERTICAL_BORDER_ROWS);
    let interior_width = geometry
        .width
        .saturating_sub(OVERLAY_HORIZONTAL_BORDER_COLUMNS);
    if interior_height == 0 || interior_width == 0 {
        return None;
    }

    let row = row.min(interior_height.saturating_sub(1));
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
    use super::{Compositor, OverlayFrame, OverlayLine, BANNER_ROWS};

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

    fn render_string(compositor: &mut Compositor, banner: &str) -> String {
        String::from_utf8(compositor.render(banner)).expect("frame is utf8")
    }

    fn move_to(row: u16, col: u16) -> String {
        format!("\x1b[{row};{col}H")
    }

    fn test_overlay() -> OverlayFrame {
        OverlayFrame {
            title: "Menu".to_owned(),
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
    fn grid_size_reserves_the_banner_row() {
        let compositor = Compositor::new(80, 24);

        assert_eq!(compositor.grid_size(), (23, 80));
    }

    #[test]
    fn rows_are_clamped_to_leave_a_usable_grid() {
        let compositor = Compositor::new(80, 1);

        // One physical row is clamped up so the grid keeps at least one row.
        assert_eq!(compositor.grid_size(), (1, 80));
    }

    #[test]
    fn first_frame_establishes_region_origin_and_banner() {
        let mut compositor = Compositor::new(40, 10);
        compositor.feed(b"hello");

        let frame = render_string(&mut compositor, "[kill]");

        // Scroll region reserves rows 2..=10 for the agent grid.
        assert!(
            frame.contains("\x1b[2;10r"),
            "first frame must reserve the banner row via a scroll region: {frame:?}"
        );
        // Banner is drawn on the physical top row with origin mode off.
        assert!(
            frame.contains("\x1b[?6l"),
            "banner must be drawn with origin mode disabled: {frame:?}"
        );
        assert!(
            frame.contains("\x1b[1;1H\x1b[7m\x1b[2K[kill]"),
            "banner text must render reverse-video on row 1: {frame:?}"
        );
        // Origin mode is enabled before the grid so offsets are automatic.
        assert!(
            frame.contains("\x1b[?6h"),
            "grid must render with origin mode enabled: {frame:?}"
        );
        // The fed content lands on the first grid row (grid row 1 under origin).
        assert!(
            frame.contains("\x1b[1;1H") && frame.contains("hello"),
            "agent output must appear on the first grid row: {frame:?}"
        );
    }

    #[test]
    fn alternate_screen_agent_output_keeps_banner_and_swallows_raw_control() {
        let mut compositor = Compositor::new(40, 10);
        // A full-screen TUI: enter the alternate screen, home the cursor, draw.
        compositor.feed(b"\x1b[?1049h\x1b[2J\x1b[1;1HTUI");

        let frame = render_string(&mut compositor, "banner");

        // The raw alternate-screen switch must never reach the physical
        // terminal; it was absorbed into the grid.
        assert!(
            !frame.contains("\x1b[?1049h"),
            "alternate-screen switch must be swallowed, not forwarded: {frame:?}"
        );
        // The banner still owns row 1.
        assert!(
            frame.contains("\x1b[1;1H\x1b[7m\x1b[2Kbanner"),
            "banner must survive a full-screen TUI frame: {frame:?}"
        );
        // The TUI content is composited into the reserved region.
        assert!(
            frame.contains("TUI"),
            "alternate-screen content must be composited: {frame:?}"
        );
        assert!(
            frame.contains("\x1b[2;10r"),
            "the reserved region must be present for the TUI frame: {frame:?}"
        );
    }

    #[test]
    fn input_modes_are_propagated_to_the_physical_terminal() {
        let mut compositor = Compositor::new(40, 10);
        // Application cursor keys + bracketed paste, as a TUI would enable.
        compositor.feed(b"\x1b[?1h\x1b[?2004h");

        let frame = render_string(&mut compositor, "b");

        assert!(
            frame.contains("\x1b[?1h"),
            "application cursor mode must reach the terminal: {frame:?}"
        );
        assert!(
            frame.contains("\x1b[?2004h"),
            "bracketed paste must reach the terminal: {frame:?}"
        );
    }

    #[test]
    fn second_frame_diffs_only_changed_rows_without_resetting_region() {
        let mut compositor = Compositor::new(40, 6);
        compositor.feed(b"line one\r\nline two");
        let _ = compositor.render("b");

        // Change only the second grid row.
        compositor.feed(b"\r\nline two changed");
        let frame = render_string(&mut compositor, "b");

        assert!(
            !frame.contains("\x1b[2;6r"),
            "an incremental frame must not re-establish the scroll region: {frame:?}"
        );
        assert!(
            frame.contains("changed"),
            "the changed row must be repainted: {frame:?}"
        );
        assert!(
            !frame.contains("line one"),
            "unchanged rows must not be repainted: {frame:?}"
        );
    }

    #[test]
    fn banner_is_not_redrawn_when_unchanged() {
        let mut compositor = Compositor::new(40, 6);
        compositor.feed(b"x");
        let _ = compositor.render("same");

        compositor.feed(b"y");
        let frame = render_string(&mut compositor, "same");

        assert!(
            !frame.contains("\x1b[7m\x1b[2K"),
            "an unchanged banner must not be redrawn: {frame:?}"
        );
    }

    #[test]
    fn changed_banner_is_redrawn_on_an_incremental_frame() {
        let mut compositor = Compositor::new(40, 6);
        compositor.feed(b"x");
        let _ = compositor.render("first");

        let frame = render_string(&mut compositor, "second");

        assert!(
            frame.contains("\x1b[1;1H\x1b[7m\x1b[2Ksecond"),
            "a changed banner must be redrawn on the top row: {frame:?}"
        );
    }

    #[test]
    fn banner_is_clamped_to_the_terminal_width() {
        let mut compositor = Compositor::new(4, 6);
        compositor.feed(b"x");

        let frame = render_string(&mut compositor, "abcdefgh");

        assert!(
            frame.contains("\x1b[2Kabcd\x1b[m"),
            "banner must be clamped to the terminal width: {frame:?}"
        );
        assert!(
            !frame.contains("abcde"),
            "banner must not exceed the terminal width: {frame:?}"
        );
    }

    #[test]
    fn resize_forces_a_full_repaint_with_the_new_region() {
        let mut compositor = Compositor::new(40, 6);
        compositor.feed(b"content");
        let _ = compositor.render("b");

        compositor.resize(50, 12);
        assert_eq!(compositor.grid_size(), (11, 50));

        let frame = render_string(&mut compositor, "b");
        assert!(
            frame.contains("\x1b[2;12r"),
            "resize must re-establish the scroll region at the new height: {frame:?}"
        );
        assert!(
            frame.contains("\x1b[1;1H\x1b[7m\x1b[2Kb"),
            "resize must redraw the banner: {frame:?}"
        );
    }

    #[test]
    fn reset_restores_region_origin_attrs_and_cursor() {
        let mut compositor = Compositor::new(40, 8);
        // Enable an input mode so reset has something to undo.
        compositor.feed(b"\x1b[?1h");
        let _ = compositor.render("b");

        let teardown = String::from_utf8(compositor.reset()).expect("teardown is utf8");

        assert!(
            teardown.contains("\x1b[?1l"),
            "reset must disable input modes the agent enabled: {teardown:?}"
        );
        assert!(
            teardown.contains("\x1b[r"),
            "reset must clear the scroll region: {teardown:?}"
        );
        assert!(
            teardown.contains("\x1b[?6l"),
            "reset must disable origin mode: {teardown:?}"
        );
        assert!(
            teardown.contains("\x1b[?25h"),
            "reset must restore the cursor: {teardown:?}"
        );
        assert!(
            teardown.contains("\x1b[8;1H"),
            "reset must park the cursor on the last physical row: {teardown:?}"
        );
    }

    #[test]
    fn hidden_cursor_is_respected_across_the_frame() {
        let mut compositor = Compositor::new(40, 6);
        compositor.feed(b"\x1b[?25l");

        let frame = render_string(&mut compositor, "b");

        assert!(
            frame.trim_end().ends_with("\x1b[?25l"),
            "a hidden agent cursor must stay hidden after the frame: {frame:?}"
        );
    }

    #[test]
    fn banner_rows_constant_is_one_physical_row() {
        assert_eq!(BANNER_ROWS, 1);
    }

    #[test]
    fn overlay_draws_centered_within_the_agent_region() {
        let mut compositor = Compositor::new(SHORT_TEST_COLS, SHORT_TEST_ROWS);
        compositor.set_overlay(Some(test_overlay()));

        let frame = render_string(&mut compositor, "b");

        assert!(
            frame.contains(&move_to(SHORT_OVERLAY_TOP_ROW, SHORT_OVERLAY_LEFT_COL)),
            "overlay must start centered in physical agent rows: {frame:?}"
        );
        assert!(
            !frame.contains("\x1b[1;4H+"),
            "overlay must not draw on the banner row: {frame:?}"
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

        let frame = render_string(&mut compositor, "b");

        assert!(
            frame.contains(&move_to(BANNER_ROWS + 1, 1)),
            "clipped overlay must start at the first agent row: {frame:?}"
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
    fn grid_diff_under_the_overlay_is_overdrawn_in_the_same_frame() {
        let mut compositor = Compositor::new(SHORT_TEST_COLS, SHORT_TEST_ROWS);
        compositor.set_overlay(Some(test_overlay()));
        let _ = compositor.render("b");

        compositor.feed(b"\x1b[1;1HUNDERLAY");
        let frame = render_string(&mut compositor, "b");

        let grid_index = frame
            .find("UNDERLAY")
            .expect("changed grid row must be included in the diff frame");
        let overlay_index = frame
            .rfind("Menu")
            .expect("overlay must be redrawn after the grid diff");
        assert!(
            grid_index < overlay_index,
            "grid diff must be emitted before overlay bytes: {frame:?}"
        );
    }

    #[test]
    fn opening_overlay_invalidates_the_diff_baseline() {
        let mut compositor = Compositor::new(SHORT_TEST_COLS, SHORT_TEST_ROWS);
        compositor.feed(b"base");
        let _ = compositor.render("b");

        compositor.set_overlay(Some(test_overlay()));
        let frame = render_string(&mut compositor, "b");

        assert!(
            frame.contains("\x1b[2;8r"),
            "opening an overlay must force a full repaint: {frame:?}"
        );
        assert!(
            frame.contains("base"),
            "opening repaint must include the existing grid contents: {frame:?}"
        );
    }

    #[test]
    fn updating_open_overlay_keeps_the_diff_baseline() {
        let mut compositor = Compositor::new(SHORT_TEST_COLS, SHORT_TEST_ROWS);
        compositor.set_overlay(Some(test_overlay()));
        let _ = compositor.render("b");

        let mut updated = test_overlay();
        updated.lines[0].highlighted = false;
        updated.lines[1].highlighted = true;
        compositor.set_overlay(Some(updated));
        let frame = render_string(&mut compositor, "b");

        assert!(
            !frame.contains("\x1b[2;8r"),
            "same-presence overlay updates must not force a full repaint: {frame:?}"
        );
        assert!(
            frame.contains("Detach"),
            "updated overlay content must still be redrawn: {frame:?}"
        );
    }

    #[test]
    fn closing_overlay_invalidates_the_diff_baseline() {
        let mut compositor = Compositor::new(SHORT_TEST_COLS, SHORT_TEST_ROWS);
        compositor.feed(b"covered");
        compositor.set_overlay(Some(test_overlay()));
        let _ = compositor.render("b");

        compositor.set_overlay(None);
        let frame = render_string(&mut compositor, "b");

        assert!(
            frame.contains("\x1b[2;8r"),
            "closing an overlay must force a full repaint: {frame:?}"
        );
        assert!(
            frame.contains("covered"),
            "closing repaint must restore covered grid contents: {frame:?}"
        );
    }

    #[test]
    fn overlay_hides_cursor_when_no_overlay_cursor_is_set() {
        let mut compositor = Compositor::new(SHORT_TEST_COLS, SHORT_TEST_ROWS);
        compositor.set_overlay(Some(test_overlay()));

        let frame = render_string(&mut compositor, "b");

        assert!(
            frame.trim_end().ends_with("\x1b[?25l"),
            "overlay without a cursor must leave the physical cursor hidden: {frame:?}"
        );
    }

    #[test]
    fn overlay_cursor_is_parked_inside_the_box_and_shown() {
        let mut compositor = Compositor::new(SHORT_TEST_COLS, SHORT_TEST_ROWS);
        let mut overlay = test_overlay();
        overlay.cursor = Some((1, 2));
        compositor.set_overlay(Some(overlay));

        let frame = render_string(&mut compositor, "b");

        assert!(
            frame.contains(&format!(
                "\x1b[?6l{}\x1b[?25h",
                move_to(CURSOR_TEST_ROW, CURSOR_TEST_COL)
            )),
            "overlay cursor must use absolute physical coordinates and be shown: {frame:?}"
        );
    }
}
