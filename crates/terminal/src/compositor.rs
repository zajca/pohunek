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

// Rust guideline compliant 2026-06-26

use std::fmt;

/// Physical rows reserved for the banner at the top of the terminal.
///
/// The agent grid is the terminal height minus this. Changing it would require
/// widening the reserved region and re-deriving the scroll-region top row.
const BANNER_ROWS: u16 = 1;

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
// Reverse video for the banner, cleared to end of line so stale text never
// shows through when the banner shrinks.
const BANNER_OPEN: &[u8] = b"\x1b[7m\x1b[2K";

/// Composites a banner row above a live agent grid for `pohunek attach`.
///
/// Feed raw PTY bytes with [`Compositor::feed`], then obtain physical-terminal
/// bytes with [`Compositor::render`]. Call [`Compositor::resize`] on window
/// changes and [`Compositor::reset`] when detaching to restore the terminal.
pub struct Compositor {
    parser: vt100::Parser,
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
            out.extend_from_slice(b"\x1b[K");
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

    fn write_cursor(&self, out: &mut Vec<u8>) {
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
    index.saturating_add(1)
}

fn push_move(out: &mut Vec<u8>, row: u16, col: u16) {
    out.extend_from_slice(format!("\x1b[{row};{col}H").as_bytes());
}

/// Draws the banner text on the physical top row, clamped to `cols`.
///
/// The caller is responsible for having origin mode disabled so `\x1b[1;1H`
/// addresses the physical top row.
fn write_banner(out: &mut Vec<u8>, banner: &str, cols: u16) {
    out.extend_from_slice(b"\x1b[1;1H");
    out.extend_from_slice(BANNER_OPEN);
    let clamped: String = banner.chars().take(usize::from(cols)).collect();
    out.extend_from_slice(clamped.as_bytes());
    out.extend_from_slice(RESET_ATTRS);
}

#[cfg(test)]
mod tests {
    use super::{Compositor, BANNER_ROWS};

    fn render_string(compositor: &mut Compositor, banner: &str) -> String {
        String::from_utf8(compositor.render(banner)).expect("frame is utf8")
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
}
