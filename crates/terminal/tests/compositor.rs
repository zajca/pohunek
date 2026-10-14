//! Agent output, the modal compositor, and a physical terminal form one attach flow.
//!
//! Every scenario drives [`Compositor`] the way the attach client does: agent
//! bytes reach both the compositor's shadow grid and a `vt100` parser standing
//! in for the physical terminal, bytes produced while a modal is open are held
//! back and replayed after [`Compositor::restore`], and assertions read the
//! physical parser's cells, attributes, cursor, and modes. A second parser that
//! only ever receives agent bytes supplies the agent-authored screen the
//! physical terminal must match once the modal is gone.

use pohunek_terminal::{Compositor, OverlayFrame, OverlayLine, MIN_ROWS};

/// The physical terminal keeps no scrollback, matching the compositor grid.
const TERMINAL_SCROLLBACK_LINES: usize = 0;
/// Private modes the compositor itself may toggle: origin mode off for
/// absolute drawing, and cursor visibility. Every other private mode belongs to
/// the agent.
const COMPOSITOR_PRIVATE_MODES: [&str; 3] = ["?6l", "?25l", "?25h"];
const OVERLAY_BORDER: &str = "|";
const RED: vt100::Color = vt100::Color::Idx(1);

/// The standard overlay in a 20x8 terminal. "Esc closes" (10 cells) is the
/// widest text, so the box is 10 + 2 padding + 2 border = 14 columns, centered
/// at column (20 - 14) / 2 = 3; borders, title, two lines, and footer are 6
/// rows, centered at row (8 - 6) / 2 = 1.
const MENU_COLS: u16 = 20;
const MENU_ROWS: u16 = 8;
const MENU_TOP_ROW: u16 = 1;
const MENU_LEFT_COL: u16 = 3;
const MENU_BOX: [&str; 6] = [
    "   +------------+   ",
    "   | Menu       |   ",
    "   | Kill       |   ",
    "   | Detach     |   ",
    "   | Esc closes |   ",
    "   +------------+   ",
];
/// Overlay text starts after the left border and one padding cell.
const MENU_TEXT_COL: u16 = MENU_LEFT_COL + 2;
/// The first body line ("Kill") sits below the top border and the title.
const MENU_FIRST_BODY_ROW: u16 = MENU_TOP_ROW + 2;
/// Columns of the four-cell texts "Menu" and "Kill".
const MENU_WORD_COLS: std::ops::Range<u16> = MENU_TEXT_COL..MENU_TEXT_COL + 4;

/// One attached client: the compositor, the physical terminal it drives, and a
/// reference terminal that sees only agent output.
struct AttachedTerminal {
    compositor: Compositor,
    physical: vt100::Parser,
    agent: vt100::Parser,
    /// Agent output held back while the modal owns the physical terminal.
    pending: Vec<u8>,
    modal: bool,
}

impl AttachedTerminal {
    fn new(cols: u16, rows: u16) -> Self {
        Self {
            compositor: Compositor::new(cols, rows),
            physical: vt100::Parser::new(rows, cols, TERMINAL_SCROLLBACK_LINES),
            agent: vt100::Parser::new(rows, cols, TERMINAL_SCROLLBACK_LINES),
            pending: Vec::new(),
            modal: false,
        }
    }

    /// Agent PTY output: passed through in raw mode, held back under a modal.
    fn agent_output(&mut self, bytes: &[u8]) {
        self.compositor.feed(bytes);
        self.agent.process(bytes);
        if self.modal {
            self.pending.extend_from_slice(bytes);
        } else {
            self.physical.process(bytes);
        }
    }

    /// Paints one modal frame on the physical terminal and returns its bytes.
    fn render(&mut self) -> Vec<u8> {
        let frame = self.compositor.render();
        self.physical.process(&frame);
        self.modal = true;
        frame
    }

    /// Closes the modal and returns the restore bytes, without replaying the
    /// held-back agent output yet.
    fn restore(&mut self) -> Vec<u8> {
        let bytes = self.compositor.restore();
        self.physical.process(&bytes);
        self.modal = false;
        bytes
    }

    /// Writes the agent output held back during the modal.
    fn replay(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        self.physical.process(&pending);
    }

    /// Resizes the physical window outside a modal, as `SIGWINCH` does.
    fn resize(&mut self, cols: u16, rows: u16) {
        self.compositor.resize(cols, rows);
        self.physical.screen_mut().set_size(rows, cols);
        self.agent.screen_mut().set_size(rows, cols);
    }

    fn screen(&self) -> &vt100::Screen {
        self.physical.screen()
    }
}

fn menu() -> OverlayFrame {
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

fn menu_with_header(header: &[&str]) -> OverlayFrame {
    let mut overlay = menu();
    overlay.header = header.iter().map(|row| (*row).to_owned()).collect();
    overlay
}

/// A terminal that receives nothing but `frame`: what the frame paints alone.
fn painted_alone(frame: &[u8], cols: u16, rows: u16) -> vt100::Parser {
    let mut parser = vt100::Parser::new(rows, cols, TERMINAL_SCROLLBACK_LINES);
    parser.process(frame);
    parser
}

fn cell(screen: &vt100::Screen, row: u16, col: u16) -> &vt100::Cell {
    screen
        .cell(row, col)
        .unwrap_or_else(|| panic!("cell ({row}, {col}) lies outside the terminal"))
}

/// Text of `row`, one char per cell, so string offsets equal columns for
/// narrow text.
fn row_text(screen: &vt100::Screen, row: u16) -> String {
    let (_, cols) = screen.size();
    (0..cols)
        .map(|col| {
            let contents = cell(screen, row, col).contents();
            if contents.is_empty() {
                " ".to_owned()
            } else {
                contents.to_owned()
            }
        })
        .collect()
}

fn rows_text(screen: &vt100::Screen, rows: std::ops::Range<u16>) -> Vec<String> {
    rows.map(|row| row_text(screen, row)).collect()
}

/// Row and first column of the single on-screen occurrence of `text`.
fn locate(screen: &vt100::Screen, text: &str) -> (u16, u16) {
    let (rows, _) = screen.size();
    let found: Vec<(u16, u16)> = (0..rows)
        .filter_map(|row| {
            let col = row_text(screen, row).find(text)?;
            Some((row, u16::try_from(col).expect("column fits the terminal")))
        })
        .collect();
    assert_eq!(found.len(), 1, "expected {text:?} on exactly one row");
    found[0]
}

/// Row and column of the overlay's top-left corner: the first `+` on screen.
fn box_corner(screen: &vt100::Screen) -> (u16, u16) {
    let (rows, _) = screen.size();
    (0..rows)
        .find_map(|row| {
            let col = row_text(screen, row).find('+')?;
            Some((row, u16::try_from(col).expect("column fits the terminal")))
        })
        .expect("an overlay border is on screen")
}

/// Columns of `row` drawn in reverse video.
fn inverse_cols(screen: &vt100::Screen, row: u16) -> Vec<u16> {
    let (_, cols) = screen.size();
    (0..cols)
        .filter(|col| cell(screen, row, *col).inverse())
        .collect()
}

/// Asserts `screen` shows `text` drawn bold in red from `(row, col)`.
fn assert_bold_red(screen: &vt100::Screen, row: u16, col: u16, text: &str) {
    for (offset, expected) in (0_u16..).zip(text.chars()) {
        let drawn = cell(screen, row, col + offset);
        assert_eq!(drawn.contents(), expected.to_string());
        assert!(drawn.bold(), "{expected:?} must stay bold");
        assert_eq!(drawn.fgcolor(), RED, "{expected:?} must stay red");
    }
}

/// Parameters and final byte of every CSI sequence in `bytes`, e.g. `?6l`.
fn control_sequences(bytes: &[u8]) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = bytes;
    while let Some(start) = rest.windows(2).position(|pair| pair == b"\x1b[") {
        let body = &rest[start + 2..];
        let Some(end) = body.iter().position(|byte| (0x40..=0x7e).contains(byte)) else {
            break;
        };
        found.push(String::from_utf8_lossy(&body[..=end]).into_owned());
        rest = &body[end + 1..];
    }
    found
}

/// Asserts `bytes` leave the modes the agent owns alone: no scroll region,
/// no screen-buffer switch, and no input mode, re-enabled or not.
fn assert_agent_modes_untouched(bytes: &[u8]) {
    for sequence in control_sequences(bytes) {
        assert!(
            !sequence.ends_with('r'),
            "compositor output sets a scroll region: {sequence:?}"
        );
        assert!(
            !sequence.starts_with('?') || COMPOSITOR_PRIVATE_MODES.contains(&sequence.as_str()),
            "compositor output changes agent mode {sequence:?}"
        );
    }
}

/// Asserts every absolute cursor move in `bytes` targets a cell of a `cols` x
/// `rows` terminal. A terminal clamps a move past its edge onto the last row or
/// column, so drawing there would overwrite what the box shows.
fn assert_moves_within(bytes: &[u8], cols: u16, rows: u16) {
    let coordinate = |value: &str| -> u16 {
        if value.is_empty() {
            1
        } else {
            value.parse().expect("cursor move coordinates are numeric")
        }
    };
    for sequence in control_sequences(bytes) {
        let Some(params) = sequence.strip_suffix('H') else {
            continue;
        };
        let (row, col) = params.split_once(';').unwrap_or((params, ""));
        let (row, col) = (coordinate(row), coordinate(col));
        assert!(
            (1..=rows).contains(&row) && (1..=cols).contains(&col),
            "cursor move {sequence:?} leaves the {cols}x{rows} terminal"
        );
    }
}

/// Asserts the physical terminal shows exactly what the agent authored: every
/// cell with its attributes, the cursor, and the agent's terminal modes.
fn assert_matches_agent(terminal: &AttachedTerminal) {
    let physical = terminal.physical.screen();
    let agent = terminal.agent.screen();
    let (rows, cols) = agent.size();
    for row in 0..rows {
        for col in 0..cols {
            let (seen, expected) = (cell(physical, row, col), cell(agent, row, col));
            let at = format!("cell ({row}, {col})");
            assert_eq!(seen.contents(), expected.contents(), "{at}");
            assert_eq!(seen.is_wide(), expected.is_wide(), "{at}");
            assert_eq!(seen.fgcolor(), expected.fgcolor(), "{at}");
            assert_eq!(seen.bgcolor(), expected.bgcolor(), "{at}");
            assert_eq!(seen.bold(), expected.bold(), "{at}");
            assert_eq!(seen.dim(), expected.dim(), "{at}");
            assert_eq!(seen.italic(), expected.italic(), "{at}");
            assert_eq!(seen.underline(), expected.underline(), "{at}");
            assert_eq!(seen.inverse(), expected.inverse(), "{at}");
        }
    }
    assert_eq!(physical.cursor_position(), agent.cursor_position());
    assert_eq!(physical.hide_cursor(), agent.hide_cursor());
    assert_eq!(physical.alternate_screen(), agent.alternate_screen());
    assert_eq!(physical.application_cursor(), agent.application_cursor());
    assert_eq!(physical.bracketed_paste(), agent.bracketed_paste());
}

#[test]
fn opening_a_modal_freezes_the_agent_screen_under_the_overlay() {
    const COLS: u16 = 40;
    const ROWS: u16 = 10;
    let mut terminal = AttachedTerminal::new(COLS, ROWS);
    terminal.agent_output(b"\x1b[1;31mTUI\x1b[m\r\nrow two\x1b[10;1Hlast row");
    let entry = rows_text(terminal.agent.screen(), 0..ROWS);
    let taller = menu_with_header(&["host: local", "branch: main"]);
    terminal.compositor.set_overlay(Some(taller));

    let first = terminal.render();
    assert!(
        first.starts_with(b"\x1b7\x1b[?6l"),
        "the first frame saves the agent cursor before drawing absolutely"
    );
    assert_agent_modes_untouched(&first);
    let screen = terminal.screen();
    assert_eq!(row_text(screen, 0), entry[0], "no banner replaces row one");
    assert!(inverse_cols(screen, 0).is_empty(), "no banner on row one");
    assert_bold_red(screen, 0, 0, "TUI");
    assert_eq!(box_corner(screen), (1, 12));
    assert_eq!(&row_text(screen, 1)[12..28], "+--------------+");
    assert_eq!(locate(screen, "| host: local  |").0, 3);
    let alone = painted_alone(&first, COLS, ROWS);
    assert_eq!(
        row_text(alone.screen(), 0),
        entry[0],
        "the frame repaints the frozen agent rows itself"
    );
    assert_bold_red(alone.screen(), 0, 0, "TUI");

    terminal.agent_output(b"\x1b[1;1Hchanged\x1b[2;1Hchanged too");
    terminal.compositor.set_overlay(Some(menu()));
    let second = terminal.render();
    assert_agent_modes_untouched(&second);
    let screen = terminal.screen();
    assert!(!screen.contents().contains("changed"), "agent output waits");
    assert_bold_red(screen, 0, 0, "TUI");
    assert_eq!(box_corner(screen), (2, 13));
    assert_eq!(&row_text(screen, 2)[13..27], "+------------+");
    for row in [0, 1, 8, 9] {
        assert_eq!(
            row_text(screen, row),
            entry[usize::from(row)],
            "row {row} drops the taller box and shows the frozen background"
        );
    }
}

#[test]
fn restoring_the_modal_hands_the_physical_terminal_back_to_the_agent() {
    const COLS: u16 = 40;
    const ROWS: u16 = 8;
    /// Cursor the agent leaves after its prompt: row two, after "$ ".
    const AGENT_CURSOR: (u16, u16) = (1, 2);
    let mut terminal = AttachedTerminal::new(COLS, ROWS);
    // A full-screen TUI with its own input modes, an attribute left active for
    // its next write, and the cursor after its prompt.
    terminal.agent_output(
        b"\x1b[?1049h\x1b[?1h\x1b[?2004h\x1b[2J\x1b[1;1H\x1b[1;31mTUI\x1b[m\x1b[2;1H$ \x1b[4m",
    );
    terminal.compositor.set_overlay(Some(menu()));
    let frame = terminal.render();
    assert_agent_modes_untouched(&frame);
    assert!(terminal.screen().contents().contains("Esc closes"));
    let alone = painted_alone(&frame, COLS, ROWS);
    assert_bold_red(alone.screen(), 0, 0, "TUI");
    assert!(
        !alone.screen().alternate_screen(),
        "the frame repaints alternate-screen content without switching screens"
    );
    terminal.agent_output(b"typed");

    let restored = terminal.restore();
    assert_agent_modes_untouched(&restored);
    let screen = terminal.screen();
    assert!(!screen.contents().contains("Menu"), "the overlay is gone");
    assert!(!screen.contents().contains("typed"), "nothing replayed yet");
    assert_bold_red(screen, 0, 0, "TUI");
    assert_eq!(row_text(screen, 1).trim_end(), "$");
    assert_eq!(screen.cursor_position(), AGENT_CURSOR);
    assert!(!screen.hide_cursor(), "the agent cursor is visible again");
    assert!(screen.alternate_screen());
    assert!(screen.application_cursor());
    assert!(screen.bracketed_paste());
    assert!(terminal.compositor.restore().is_empty(), "no modal is left");

    terminal.replay();
    assert!(
        cell(terminal.screen(), 1, 2).underline(),
        "saved SGR is back"
    );
    assert_matches_agent(&terminal);
}

#[test]
fn the_agent_keeps_its_origin_mode_and_scroll_region() {
    /// First row of the agent scroll region `3;6r`, zero-based.
    const REGION_TOP_ROW: u16 = 2;
    let mut terminal = AttachedTerminal::new(MENU_COLS, MENU_ROWS);
    terminal.agent_output(b"\x1b[8;1Houtside\x1b[3;6r\x1b[?6h\x1b[1;1Hregion top");
    terminal.compositor.set_overlay(Some(menu()));

    let frame = terminal.render();
    assert_agent_modes_untouched(&frame);
    // The box keeps its absolute rows 1..=6; "region top" stays visible left
    // of it on the region's first row.
    assert_eq!(
        rows_text(terminal.screen(), 0..MENU_ROWS),
        [
            "                    ",
            MENU_BOX[0],
            "reg| Menu       |   ",
            MENU_BOX[2],
            MENU_BOX[3],
            MENU_BOX[4],
            MENU_BOX[5],
            "outside             ",
        ],
        "the overlay is drawn at absolute rows despite the agent's origin mode"
    );

    let restored = terminal.restore();
    assert_agent_modes_untouched(&restored);
    terminal.replay();
    // Origin-relative home lands on the region's first row.
    terminal.agent_output(b"\x1b[1;1HX");
    assert_eq!(cell(terminal.screen(), REGION_TOP_ROW, 0).contents(), "X");
    assert_matches_agent(&terminal);
    // A line feed on the region's last row scrolls only the region.
    terminal.agent_output(b"\x1b[4;1H\nscrolled");
    assert_eq!(
        row_text(terminal.screen(), MENU_ROWS - 1).trim_end(),
        "outside"
    );
    assert_matches_agent(&terminal);
}

#[test]
fn the_overlay_is_centered_bordered_and_highlights_only_selected_text() {
    let mut terminal = AttachedTerminal::new(MENU_COLS, MENU_ROWS);
    terminal.agent_output(b"\x1b[8;1Hagent prompt");
    terminal.compositor.set_overlay(Some(menu()));
    let frame = terminal.render();
    assert_agent_modes_untouched(&frame);
    let screen = terminal.screen();

    assert_eq!(row_text(screen, 0).trim_end(), "");
    assert_eq!(rows_text(screen, MENU_TOP_ROW..MENU_TOP_ROW + 6), MENU_BOX);
    assert_eq!(row_text(screen, 7).trim_end(), "agent prompt");
    let title_row = MENU_TOP_ROW + 1;
    for row in 0..MENU_ROWS {
        let expected: Vec<u16> = if row == title_row || row == MENU_FIRST_BODY_ROW {
            MENU_WORD_COLS.collect()
        } else {
            Vec::new()
        };
        assert_eq!(
            inverse_cols(screen, row),
            expected,
            "only the title and the selected line's text are reversed (row {row})"
        );
    }
}

#[test]
fn header_rows_sit_between_title_and_body_and_yield_to_body_rows() {
    let mut terminal = AttachedTerminal::new(MENU_COLS, MENU_ROWS);
    terminal
        .compositor
        .set_overlay(Some(menu_with_header(&["host: local", "branch: main"])));
    terminal.render();
    // "branch: main" (12 cells) widens the box to 16 columns at column 2, and
    // the two header rows grow it to the full 8 rows.
    assert_eq!(
        rows_text(terminal.screen(), 0..MENU_ROWS),
        [
            "  +--------------+  ",
            "  | Menu         |  ",
            "  | host: local  |  ",
            "  | branch: main |  ",
            "  | Kill         |  ",
            "  | Detach       |  ",
            "  | Esc closes   |  ",
            "  +--------------+  ",
        ]
    );
    for header_row in [2, 3] {
        assert!(inverse_cols(terminal.screen(), header_row).is_empty());
    }

    let mut updated = AttachedTerminal::new(MENU_COLS, MENU_ROWS);
    updated
        .compositor
        .set_overlay_header(vec!["ignored".to_owned()]);
    updated.compositor.set_overlay(Some(menu()));
    updated
        .compositor
        .set_overlay_header(vec!["host: local".to_owned()]);
    updated.render();
    assert!(!updated.screen().contents().contains("ignored"));
    assert_eq!(
        rows_text(updated.screen(), 0..7),
        [
            "  +-------------+   ",
            "  | Menu        |   ",
            "  | host: local |   ",
            "  | Kill        |   ",
            "  | Detach      |   ",
            "  | Esc closes  |   ",
            "  +-------------+   ",
        ]
    );

    // Borders, title, two lines, and footer need 6 of the 7 rows, so only the
    // first header row fits.
    let mut short = AttachedTerminal::new(MENU_COLS, 7);
    short
        .compositor
        .set_overlay(Some(menu_with_header(&["first", "second", "third"])));
    short.render();
    assert_eq!(
        rows_text(short.screen(), 0..7),
        [
            "   +------------+   ",
            "   | Menu       |   ",
            "   | first      |   ",
            "   | Kill       |   ",
            "   | Detach     |   ",
            "   | Esc closes |   ",
            "   +------------+   ",
        ]
    );
}

#[test]
fn the_overlay_is_clipped_to_a_terminal_smaller_than_the_box() {
    let mut tiny = AttachedTerminal::new(10, 4);
    tiny.compositor.set_overlay(Some(OverlayFrame {
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
    let frame = tiny.render();
    assert_agent_modes_untouched(&frame);
    assert_moves_within(&frame, 10, 4);
    // A frame drawing past the last row would be clamped onto it by the
    // terminal and overwrite the bottom border.
    assert_eq!(
        rows_text(tiny.screen(), 0..4),
        ["+--------+", "| Very l |", "| First  |", "+--------+"]
    );

    let mut narrow = AttachedTerminal::new(10, MENU_ROWS);
    narrow
        .compositor
        .set_overlay(Some(menu_with_header(&["branch: a-very-long-branch-name"])));
    let frame = narrow.render();
    assert_moves_within(&frame, 10, MENU_ROWS);
    assert_eq!(
        rows_text(narrow.screen(), 0..MENU_ROWS),
        [
            "+--------+",
            "| Menu   |",
            "| branch |",
            "| Kill   |",
            "| Detach |",
            "| Esc cl |",
            "+--------+",
            "          ",
        ]
    );

    // A zero-row window still leaves one addressable row for the box.
    let mut rowless = Compositor::new(MENU_COLS, 0);
    assert_eq!(rowless.grid_size(), (MIN_ROWS, MENU_COLS));
    rowless.set_overlay(Some(menu()));
    let frame = rowless.render();
    assert_moves_within(&frame, MENU_COLS, MIN_ROWS);
    let alone = painted_alone(&frame, MENU_COLS, MIN_ROWS);
    assert_eq!(row_text(alone.screen(), 0), MENU_BOX[0]);
}

#[test]
fn wide_header_characters_take_two_cells_and_are_never_split() {
    /// Six wide characters: 12 cells, wider than the 10-cell footer.
    const WIDE_HEADER: &str = "ああああああ";
    /// 12 cells + 2 padding + 2 border = 16 columns, centered at column 2.
    const LEFT_COL: u16 = 2;
    const RIGHT_COL: u16 = LEFT_COL + 15;
    const HEADER_ROW: u16 = 2;
    let mut terminal = AttachedTerminal::new(MENU_COLS, MENU_ROWS);
    terminal
        .compositor
        .set_overlay(Some(menu_with_header(&[WIDE_HEADER])));
    terminal.render();
    let screen = terminal.screen();
    // Each wide cell is followed by an empty continuation cell.
    assert_eq!(row_text(screen, HEADER_ROW), "  | あ あ あ あ あ あ  |  ");
    for index in 0..6 {
        let wide = cell(screen, HEADER_ROW, LEFT_COL + 2 + 2 * index);
        assert_eq!(wide.contents(), "あ");
        assert!(wide.is_wide());
    }
    for row in [1, HEADER_ROW, 3] {
        assert_eq!(
            cell(screen, row, RIGHT_COL).contents(),
            OVERLAY_BORDER,
            "right border of row {row} stays aligned"
        );
    }

    // A 7-column terminal leaves 3 text cells: one wide character fits, the
    // second would cross the edge and is dropped whole.
    let mut narrow = AttachedTerminal::new(7, MENU_ROWS);
    narrow
        .compositor
        .set_overlay(Some(menu_with_header(&["ああ"])));
    narrow.render();
    let screen = narrow.screen();
    assert_eq!(row_text(screen, 1), "| Men |");
    assert_eq!(row_text(screen, HEADER_ROW), "| あ   |");
    assert!(cell(screen, HEADER_ROW, 2).is_wide());
    assert!(cell(screen, HEADER_ROW, 3).is_wide_continuation());
    assert!(!cell(screen, HEADER_ROW, 4).is_wide());
}

#[test]
fn the_physical_cursor_follows_the_overlay_input_cell_or_the_agent() {
    /// Agent cursor after "agent prompt" on the first row.
    const AGENT_CURSOR: (u16, u16) = (0, 12);
    /// Overlay cursor cell two columns into the text of the first body line.
    const INPUT_CELL: (u16, u16) = (0, 2);
    /// That cell on the physical terminal: the "Kill" row, after the left
    /// border, the interior's padding cell, and two text columns.
    const INPUT_CURSOR: (u16, u16) = (MENU_FIRST_BODY_ROW, MENU_LEFT_COL + 1 + 1 + 2);
    let mut terminal = AttachedTerminal::new(MENU_COLS, MENU_ROWS);
    terminal.agent_output(b"agent prompt");
    terminal.compositor.set_overlay(Some(menu()));
    terminal.render();
    assert!(
        terminal.screen().hide_cursor(),
        "an overlay without an input cell hides the visible agent cursor"
    );

    let mut input = menu();
    input.cursor = Some(INPUT_CELL);
    terminal.compositor.set_overlay(Some(input));
    terminal.render();
    let screen = terminal.screen();
    assert_eq!(locate(screen, "Kill").0, INPUT_CURSOR.0);
    assert_eq!(screen.cursor_position(), INPUT_CURSOR);
    assert!(!screen.hide_cursor());

    // One header row makes the box 7 rows tall from row 0, which keeps the
    // first body line on the same physical row below the header.
    let mut input = menu_with_header(&["host"]);
    input.cursor = Some(INPUT_CELL);
    terminal.compositor.set_overlay(Some(input));
    terminal.render();
    let screen = terminal.screen();
    assert_eq!(cell(screen, 0, MENU_LEFT_COL).contents(), "+");
    assert_eq!(locate(screen, "| host").0, INPUT_CURSOR.0 - 1);
    assert_eq!(locate(screen, "Kill").0, INPUT_CURSOR.0);
    assert_eq!(screen.cursor_position(), INPUT_CURSOR);
    assert!(!screen.hide_cursor());

    terminal.compositor.set_overlay(None);
    terminal.render();
    assert_eq!(terminal.screen().cursor_position(), AGENT_CURSOR);
    assert!(!terminal.screen().hide_cursor(), "the agent cursor shows");
    terminal.agent_output(b"\x1b[?25l");
    terminal.render();
    assert_eq!(terminal.screen().cursor_position(), AGENT_CURSOR);
    assert!(
        terminal.screen().hide_cursor(),
        "a cursor the agent hid stays hidden"
    );

    let mut hidden = AttachedTerminal::new(MENU_COLS, MENU_ROWS);
    hidden.agent_output(b"agent prompt\x1b[?25l");
    let mut input = menu();
    input.cursor = Some(INPUT_CELL);
    hidden.compositor.set_overlay(Some(input));
    hidden.render();
    assert!(!hidden.screen().hide_cursor(), "the input cell is shown");
    hidden.restore();
    assert!(
        hidden.screen().hide_cursor(),
        "restore hides the cursor the agent had hidden"
    );
    assert_matches_agent(&hidden);
}

#[test]
fn resizing_after_a_modal_repaints_the_full_new_grid() {
    const COLS: u16 = 50;
    const ROWS: u16 = 12;
    let mut terminal = AttachedTerminal::new(40, 6);
    terminal.agent_output(b"content");
    terminal.compositor.set_overlay(Some(menu()));
    terminal.render();
    terminal.restore();
    terminal.replay();

    terminal.resize(COLS, ROWS);
    assert_eq!(terminal.compositor.grid_size(), (ROWS, COLS));
    terminal.agent_output(b"\x1b[12;1Hbottom");
    terminal.compositor.set_overlay(Some(menu()));
    let frame = terminal.render();
    assert_agent_modes_untouched(&frame);
    // The standard box is centered in the new 50x12 grid.
    assert_eq!(box_corner(terminal.screen()), (3, 18));
    let alone = painted_alone(&frame, COLS, ROWS);
    assert_eq!(row_text(alone.screen(), 0).trim_end(), "content");
    assert_eq!(row_text(alone.screen(), ROWS - 1).trim_end(), "bottom");
}
