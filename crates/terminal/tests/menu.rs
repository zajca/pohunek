//! Menu state and terminal composition are exercised as one attach interaction.
//!
//! Every scenario drives [`MenuState`] through [`step`] and paints each state the
//! way the attach client does: the compositor's bytes go to a `vt100` parser
//! standing in for the physical terminal, and assertions read that parser's
//! cells, attributes, and cursor.

use pohunek_terminal::{step, Compositor, MenuEffect, MenuEvent, MenuKey, MenuOutcome, MenuState};

const TERMINAL_COLS: u16 = 120;
const TERMINAL_ROWS: u16 = 24;
/// The physical terminal keeps no scrollback, matching the compositor grid.
const TERMINAL_SCROLLBACK_LINES: usize = 0;
const EXPECTED_SESSION_NAME_LIMIT_BYTES: usize = 128;
/// Agent output already on screen when the menu opens; it leaves the agent
/// cursor on the first row, right after the prompt text.
const AGENT_PROMPT: &str = "agent prompt";
const AGENT_PROMPT_ROW: u16 = 0;
/// Left and right edge of every overlay text row.
const OVERLAY_BORDER: char = '|';
const RENAME_INPUT_PREFIX: &str = "Name: ";

const ROOT_KILL: &str = "k  Kill session";
const ROOT_REMOVE: &str = "t  Terminate and delete session";
const ROOT_DETACH: &str = "d  Detach";
const ROOT_NEW_SESSION: &str = "n  New session in this worktree";
const ROOT_FORK: &str = "f  Fork session";
const ROOT_RENAME: &str = "r  Rename session";
/// Root rows in display order.
const ROOT_ROWS: [&str; 6] = [
    ROOT_KILL,
    ROOT_REMOVE,
    ROOT_DETACH,
    ROOT_NEW_SESSION,
    ROOT_FORK,
    ROOT_RENAME,
];

/// One attached client: the compositor plus the physical terminal it drives.
struct AttachedTerminal {
    compositor: Compositor,
    physical: vt100::Parser,
}

impl AttachedTerminal {
    /// Starts from raw passthrough, where agent bytes reach both the
    /// compositor's shadow grid and the physical terminal.
    fn new() -> Self {
        let mut compositor = Compositor::new(TERMINAL_COLS, TERMINAL_ROWS);
        let mut physical =
            vt100::Parser::new(TERMINAL_ROWS, TERMINAL_COLS, TERMINAL_SCROLLBACK_LINES);
        compositor.feed(AGENT_PROMPT.as_bytes());
        physical.process(AGENT_PROMPT.as_bytes());
        Self {
            compositor,
            physical,
        }
    }

    /// Paints `state` like the attach client: a closed menu restores the agent
    /// screen, every other state repaints the overlay.
    fn show(&mut self, state: &MenuState) -> &vt100::Screen {
        self.compositor.set_overlay(state.to_overlay_frame());
        let bytes = if *state == MenuState::Closed {
            self.compositor.restore()
        } else {
            self.compositor.render()
        };
        self.physical.process(&bytes);
        self.physical.screen()
    }
}

/// Physical screen after a fresh attach paints `state`.
fn composed(state: &MenuState) -> vt100::Screen {
    AttachedTerminal::new().show(state).clone()
}

/// Text of `row`, one char per cell, so string offsets equal columns.
fn row_text(screen: &vt100::Screen, row: u16) -> String {
    (0..TERMINAL_COLS)
        .map(|col| {
            screen
                .cell(row, col)
                .map(vt100::Cell::contents)
                .filter(|contents| !contents.is_empty())
                .unwrap_or(" ")
                .to_owned()
        })
        .collect()
}

/// Row and first column of the single on-screen occurrence of `text`.
fn locate(screen: &vt100::Screen, text: &str) -> (u16, u16) {
    let found: Vec<(u16, u16)> = (0..TERMINAL_ROWS)
        .filter_map(|row| {
            let col = row_text(screen, row).find(text)?;
            Some((row, u16::try_from(col).expect("column fits the terminal")))
        })
        .collect();
    assert_eq!(found.len(), 1, "expected {text:?} on exactly one row");
    found[0]
}

/// Whether every cell of the on-screen `text` is drawn in reverse video.
fn is_reverse(screen: &vt100::Screen, text: &str) -> bool {
    let (row, start) = locate(screen, text);
    let len = u16::try_from(text.len()).expect("label fits the terminal");
    (start..start + len).all(|col| screen.cell(row, col).is_some_and(vt100::Cell::inverse))
}

/// Root rows currently drawn in reverse video.
fn highlighted_root_rows(screen: &vt100::Screen) -> Vec<&'static str> {
    ROOT_ROWS
        .into_iter()
        .filter(|label| is_reverse(screen, label))
        .collect()
}

/// Asserts the physical cursor is visible on the rename input row, inside the
/// box, and between the start of the input text and the cell after it.
/// Returns the cursor column.
fn assert_rename_cursor(screen: &vt100::Screen, buffer: &str) -> u16 {
    let input = format!("{RENAME_INPUT_PREFIX}{buffer}");
    let (input_row, input_col) = locate(screen, &input);
    let input_len = u16::try_from(input.len()).expect("input fits the terminal");
    let (cursor_row, cursor_col) = screen.cursor_position();
    assert!(!screen.hide_cursor(), "rename input must show the cursor");
    assert_eq!(cursor_row, input_row, "cursor must sit on the input row");
    assert!(
        (input_col..=input_col + input_len).contains(&cursor_col),
        "cursor column {cursor_col} must stay within {input:?} starting at {input_col}"
    );
    let text = row_text(screen, input_row);
    let left_border = text
        .find(OVERLAY_BORDER)
        .expect("input row has a left border");
    let right_border = text
        .rfind(OVERLAY_BORDER)
        .expect("input row has a right border");
    assert!(
        (left_border + 1..right_border).contains(&usize::from(cursor_col)),
        "cursor column {cursor_col} must stay inside the box interior"
    );
    cursor_col
}

/// Asserts the agent screen is back without any overlay and the agent cursor
/// is visible where the agent left it.
fn assert_agent_screen_restored(screen: &vt100::Screen, overlay_text: &str) {
    let contents = screen.contents();
    assert!(contents.contains(AGENT_PROMPT));
    assert!(!contents.contains(overlay_text));
    let prompt_end = u16::try_from(AGENT_PROMPT.len()).expect("prompt fits the terminal");
    assert_eq!(screen.cursor_position(), (AGENT_PROMPT_ROW, prompt_end));
    assert!(
        !screen.hide_cursor(),
        "the agent cursor must be visible again"
    );
}

#[test]
fn navigation_and_session_actions_render_their_progress_and_results() {
    let mut terminal = AttachedTerminal::new();
    let root = MenuState::open_root();
    let screen = terminal.show(&root);
    let contents = screen.contents();
    for label in ["Session menu", "Enter select  Esc close"]
        .into_iter()
        .chain(ROOT_ROWS)
    {
        assert!(contents.contains(label), "menu omitted {label:?}");
    }
    assert!(is_reverse(screen, "Session menu"));
    assert_eq!(highlighted_root_rows(screen), [ROOT_KILL]);
    assert!(screen.hide_cursor(), "the root menu must hide the cursor");

    let (down, effects) = step(root.clone(), MenuEvent::Key(MenuKey::Down));
    assert_eq!(down, MenuState::Root { selected: 1 });
    assert!(effects.is_empty());
    let screen = terminal.show(&down);
    assert_eq!(highlighted_root_rows(screen), [ROOT_REMOVE]);
    assert!(screen.hide_cursor());
    let (up, effects) = step(down.clone(), MenuEvent::Key(MenuKey::Up));
    assert_eq!(up, root);
    assert!(effects.is_empty());
    assert_eq!(highlighted_root_rows(terminal.show(&up)), [ROOT_KILL]);
    let (remove_prompt, effects) = step(down, MenuEvent::Key(MenuKey::Enter));
    assert_eq!(remove_prompt, MenuState::ConfirmRemove);
    assert!(effects.is_empty());
    assert!(terminal
        .show(&remove_prompt)
        .contents()
        .contains("Terminate and delete"));
    let (returned, effects) = step(remove_prompt, MenuEvent::Key(MenuKey::Esc));
    assert_eq!(returned, root);
    assert!(effects.is_empty());
    assert_eq!(highlighted_root_rows(terminal.show(&returned)), [ROOT_KILL]);

    let mut selected = root.clone();
    for highlighted in [ROOT_REMOVE, ROOT_DETACH, ROOT_NEW_SESSION] {
        let (next, effects) = step(selected, MenuEvent::Key(MenuKey::Down));
        assert!(effects.is_empty());
        assert_eq!(highlighted_root_rows(terminal.show(&next)), [highlighted]);
        selected = next;
    }
    let (busy, effects) = step(selected, MenuEvent::Key(MenuKey::Enter));
    assert_eq!(effects, vec![MenuEffect::RunNewSession]);
    let screen = terminal.show(&busy);
    assert!(screen.contents().contains("Starting session"));
    assert!(screen.hide_cursor(), "a busy menu must hide the cursor");
    let (back, _) = step(
        busy,
        MenuEvent::RpcDone(MenuOutcome::NewSession {
            id: "s-9".to_owned(),
        }),
    );
    let (back, _) = step(back, MenuEvent::Key(MenuKey::Esc));
    assert_eq!(back, root);

    for (key, effect, busy_text, result, result_text) in [
        (
            b'n',
            MenuEffect::RunNewSession,
            "Starting session",
            MenuOutcome::NewSession {
                id: "s-10".to_owned(),
            },
            "New session created: s-10",
        ),
        (
            b'f',
            MenuEffect::RunFork,
            "Forking session",
            MenuOutcome::Forked {
                id: "s-11".to_owned(),
            },
            "Forked session created: s-11",
        ),
    ] {
        let (busy, effects) = step(root.clone(), MenuEvent::Key(MenuKey::Byte(key)));
        assert_eq!(effects, vec![effect]);
        assert!(terminal.show(&busy).contents().contains(busy_text));
        let (result_state, effects) = step(busy, MenuEvent::RpcDone(result));
        assert!(effects.is_empty());
        let screen = terminal.show(&result_state);
        assert!(screen.contents().contains(result_text));
        assert!(screen.hide_cursor(), "a result must hide the cursor");
        let (returned, effects) = step(result_state, MenuEvent::Key(MenuKey::Esc));
        assert_eq!(returned, root);
        assert!(effects.is_empty());
    }

    let (closed, effects) = step(root, MenuEvent::Key(MenuKey::Byte(b'd')));
    assert_eq!(effects, vec![MenuEffect::RunDetach, MenuEffect::Close]);
    assert_eq!(closed, MenuState::Closed);
    assert_agent_screen_restored(terminal.show(&closed), "Session menu");
}

#[test]
fn destructive_actions_require_confirmation_before_the_menu_runs_them() {
    let mut terminal = AttachedTerminal::new();
    let (kill_prompt, effects) = step(MenuState::open_root(), MenuEvent::Key(MenuKey::Byte(b'k')));
    assert_eq!(kill_prompt, MenuState::ConfirmKill);
    assert!(effects.is_empty());
    let screen = terminal.show(&kill_prompt);
    assert!(screen.contents().contains("Kill session"));
    assert!(is_reverse(screen, "y  Confirm kill"));
    assert!(!is_reverse(screen, "This stops the PTY process."));
    assert!(screen.hide_cursor(), "a confirmation must hide the cursor");
    let (root, effects) = step(kill_prompt, MenuEvent::Key(MenuKey::Byte(b'n')));
    assert_eq!(root, MenuState::open_root());
    assert!(effects.is_empty());
    let (kill_prompt, _) = step(root, MenuEvent::Key(MenuKey::Byte(b'k')));
    let (busy, effects) = step(kill_prompt, MenuEvent::Key(MenuKey::Byte(b'y')));
    assert_eq!(effects, vec![MenuEffect::RunKill]);
    assert!(terminal.show(&busy).contents().contains("Killing session"));
    let (result, effects) = step(busy, MenuEvent::RpcDone(MenuOutcome::Killed));
    assert!(effects.is_empty());
    assert!(terminal
        .show(&result)
        .contents()
        .contains("Session stopped"));

    let (root, _) = step(result, MenuEvent::Key(MenuKey::Esc));
    let (remove_prompt, effects) = step(root, MenuEvent::Key(MenuKey::Byte(b't')));
    assert_eq!(remove_prompt, MenuState::ConfirmRemove);
    assert!(effects.is_empty());
    let screen = terminal.show(&remove_prompt);
    assert!(screen.contents().contains("Terminate and delete"));
    assert!(is_reverse(screen, "y  Confirm terminate and delete"));
    assert!(!is_reverse(screen, "The branch is kept."));
    assert!(screen.hide_cursor(), "a confirmation must hide the cursor");
    let (still_prompt, effects) = step(remove_prompt, MenuEvent::Key(MenuKey::Byte(b'x')));
    assert_eq!(still_prompt, MenuState::ConfirmRemove);
    assert!(effects.is_empty());
    let (declined, effects) = step(still_prompt.clone(), MenuEvent::Key(MenuKey::Byte(b'n')));
    assert_eq!(declined, MenuState::open_root());
    assert!(effects.is_empty());
    let (busy, effects) = step(still_prompt, MenuEvent::Key(MenuKey::Byte(b'y')));
    assert_eq!(effects, vec![MenuEffect::RunRemove]);
    assert!(terminal
        .show(&busy)
        .contents()
        .contains("Terminating and deleting session"));
    let (result, effects) = step(
        busy,
        MenuEvent::RpcDone(MenuOutcome::Removed {
            worktrees_removed: 1,
            worktrees_failed: 2,
        }),
    );
    assert!(effects.is_empty());
    assert!(terminal.show(&result).contents().contains(
        "Session terminated and deleted; worktrees removed: 1; worktrees left on disk: 2"
    ));
}

#[test]
fn rename_input_is_bounded_and_the_composed_result_reports_failure() {
    let mut terminal = AttachedTerminal::new();
    let (hotkey_input, effects) = step(MenuState::open_root(), MenuEvent::Key(MenuKey::Byte(b'r')));
    assert_eq!(
        hotkey_input,
        MenuState::RenameInput {
            buffer: String::new()
        }
    );
    assert!(effects.is_empty());
    assert_rename_cursor(terminal.show(&hotkey_input), "");
    let (last_row, effects) = step(MenuState::open_root(), MenuEvent::Key(MenuKey::Up));
    assert_eq!(last_row, MenuState::Root { selected: 5 });
    assert!(effects.is_empty());
    assert_eq!(
        highlighted_root_rows(terminal.show(&last_row)),
        [ROOT_RENAME]
    );
    let (mut input, effects) = step(last_row, MenuEvent::Key(MenuKey::Enter));
    assert!(effects.is_empty());
    let screen = terminal.show(&input);
    let contents = screen.contents();
    assert!(contents.contains("Rename session"));
    assert!(contents.contains("Enter save  Esc back"));
    assert!(is_reverse(screen, RENAME_INPUT_PREFIX));
    let mut cursor_col = assert_rename_cursor(screen, "");

    let mut typed = String::new();
    for byte in b"new name" {
        let (next, effects) = step(input, MenuEvent::Key(MenuKey::Byte(*byte)));
        input = next;
        assert!(effects.is_empty());
        typed.push(char::from(*byte));
        let screen = terminal.show(&input);
        assert!(is_reverse(screen, &format!("{RENAME_INPUT_PREFIX}{typed}")));
        let next_col = assert_rename_cursor(screen, &typed);
        assert_eq!(next_col, cursor_col + 1, "cursor must advance with input");
        cursor_col = next_col;
    }
    let (input, effects) = step(input, MenuEvent::Key(MenuKey::Backspace));
    assert!(effects.is_empty());
    let screen = terminal.show(&input);
    assert!(screen.contents().contains("Name: new nam"));
    assert!(is_reverse(screen, "Name: new nam"));
    let next_col = assert_rename_cursor(screen, "new nam");
    assert_eq!(next_col + 1, cursor_col, "cursor must retreat on backspace");

    let (busy, effects) = step(input, MenuEvent::Key(MenuKey::Enter));
    assert_eq!(effects, vec![MenuEffect::RunRename("new nam".to_owned())]);
    let screen = terminal.show(&busy);
    assert!(screen.contents().contains("Renaming session"));
    assert!(
        screen.hide_cursor(),
        "leaving rename input must hide the cursor"
    );
    let (result, effects) = step(busy, MenuEvent::RpcFailed("name rejected".to_owned()));
    assert!(effects.is_empty());
    let screen = terminal.show(&result);
    assert!(screen.contents().contains("Error: name rejected"));
    assert!(screen.hide_cursor());

    let mut input = MenuState::RenameInput {
        buffer: String::new(),
    };
    for _ in 0..=EXPECTED_SESSION_NAME_LIMIT_BYTES {
        let (next, effects) = step(input, MenuEvent::Key(MenuKey::Byte(b'a')));
        input = next;
        assert!(effects.is_empty());
    }
    let MenuState::RenameInput { buffer } = &input else {
        panic!("rename input should remain open");
    };
    assert_eq!(buffer.len(), EXPECTED_SESSION_NAME_LIMIT_BYTES);
    assert!(composed(&input).contents().contains("Name: a"));
}

#[test]
fn closing_a_busy_menu_restores_the_agent_screen_and_ignores_late_results() {
    let mut terminal = AttachedTerminal::new();
    let root = MenuState::open_root();
    assert!(terminal.show(&root).contents().contains("Session menu"));
    let (closed, effects) = step(root, MenuEvent::Key(MenuKey::Esc));
    assert_eq!(effects, vec![MenuEffect::Close]);
    assert_eq!(closed, MenuState::Closed);
    assert_agent_screen_restored(terminal.show(&closed), "Session menu");

    let (busy, effects) = step(MenuState::open_root(), MenuEvent::Key(MenuKey::Byte(b'n')));
    assert_eq!(effects, vec![MenuEffect::RunNewSession]);
    let (busy, effects) = step(busy, MenuEvent::Key(MenuKey::Byte(b'r')));
    assert!(effects.is_empty());
    assert!(terminal.show(&busy).contents().contains("Starting session"));
    let (closed, effects) = step(busy, MenuEvent::Key(MenuKey::Esc));
    assert_eq!(effects, vec![MenuEffect::Close]);
    assert_eq!(closed, MenuState::Closed);
    assert_agent_screen_restored(terminal.show(&closed), "Starting session");
    let (closed, effects) = step(
        closed,
        MenuEvent::RpcDone(MenuOutcome::NewSession {
            id: "s-10".to_owned(),
        }),
    );
    assert!(effects.is_empty());
    assert_eq!(closed, MenuState::Closed);
    assert_agent_screen_restored(terminal.show(&closed), "New session created");
}
