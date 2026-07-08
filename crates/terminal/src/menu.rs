//! Headless session-menu state machine for composited attach screens.

// Rust guideline compliant 2026-07-07

use crate::{OverlayFrame, OverlayLine};

/// Root-menu row selected when the modal opens or walks back.
const ROOT_DEFAULT_SELECTED: usize = 0;
/// Number of actions shown in the attach session menu.
const ROOT_ITEM_COUNT: usize = 5;
/// Root row for killing the current session.
const ROOT_KILL_INDEX: usize = 0;
/// Root row for detaching from the current session.
const ROOT_DETACH_INDEX: usize = 1;
/// Root row for starting another session in the same worktree.
const ROOT_NEW_SESSION_INDEX: usize = 2;
/// Root row for forking the current native agent conversation.
const ROOT_FORK_INDEX: usize = 3;
/// Root row for renaming the current session.
const ROOT_RENAME_INDEX: usize = 4;
/// Cursor row for the rename body line, relative to overlay interior.
const RENAME_INPUT_CURSOR_ROW: u16 = 1;
/// Visible prompt before the editable rename buffer.
const RENAME_INPUT_PREFIX: &str = "Name: ";
/// Maximum session name accepted by the daemon validator.
///
/// Mirrors `crates/daemon/src/session/mod.rs` so pasted rename input is capped
/// before it can grow without bound. Keep this in sync with
/// `validate_session_name`.
const MAX_SESSION_NAME_BYTES: usize = 128;

/// Modal state for the composited attach session menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuState {
    /// The menu is not visible and should not own input.
    Closed,
    /// The root menu is visible with a selected row.
    Root {
        /// Zero-based selected root-menu row.
        selected: usize,
    },
    /// Kill confirmation is visible.
    ConfirmKill,
    /// Rename input is visible with the current text buffer.
    RenameInput {
        /// Editable session-name buffer.
        buffer: String,
    },
    /// An RPC-backed action is in flight.
    Busy {
        /// Human label shown while the action is pending.
        label: String,
    },
    /// A completed menu action or error is visible.
    Result {
        /// Result message rendered in the modal body.
        message: String,
    },
}

impl MenuState {
    /// Returns the default opened root menu.
    #[must_use]
    pub const fn open_root() -> Self {
        Self::Root {
            selected: ROOT_DEFAULT_SELECTED,
        }
    }

    /// Converts the state to a compositor overlay frame.
    #[must_use]
    pub fn to_overlay_frame(&self) -> Option<OverlayFrame> {
        match self {
            Self::Closed => None,
            Self::Root { selected } => Some(root_overlay(*selected)),
            Self::ConfirmKill => Some(confirm_kill_overlay()),
            Self::RenameInput { buffer } => Some(rename_overlay(buffer)),
            Self::Busy { label } => Some(busy_overlay(label)),
            Self::Result { message } => Some(result_overlay(message)),
        }
    }
}

/// Decoded input key understood by the menu state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKey {
    /// A plain byte typed by the user.
    Byte(u8),
    /// Up-arrow navigation.
    Up,
    /// Down-arrow navigation.
    Down,
    /// Enter / return.
    Enter,
    /// Escape.
    Esc,
    /// Backspace/delete-left.
    Backspace,
    /// A mouse report owned by the modal.
    Mouse,
}

/// Event delivered to the menu state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEvent {
    /// Keyboard or mouse input.
    Key(MenuKey),
    /// An RPC effect completed successfully.
    RpcDone(MenuOutcome),
    /// An RPC effect failed.
    RpcFailed(String),
}

/// Successful outcome from an RPC-backed menu action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuOutcome {
    /// A stop request completed.
    Killed,
    /// A new session was created.
    NewSession {
        /// New session id.
        id: String,
    },
    /// A session was forked.
    Forked {
        /// Forked session id.
        id: String,
    },
    /// The current session was renamed.
    Renamed {
        /// New display name, or `None` when cleared.
        name: Option<String>,
    },
}

/// Side effects requested by a state transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEffect {
    /// Send `session.stop`.
    RunKill,
    /// Send `session.detach`.
    RunDetach,
    /// Inspect the current session, then send `session.new`.
    RunNewSession,
    /// Send `session.fork`.
    RunFork,
    /// Send `session.rename`.
    RunRename(String),
    /// Close and clear the compositor overlay.
    Close,
}

/// Applies one menu event and returns the next state plus requested effects.
#[must_use]
pub fn step(state: MenuState, event: MenuEvent) -> (MenuState, Vec<MenuEffect>) {
    match (state, event) {
        (MenuState::Closed, MenuEvent::RpcDone(_) | MenuEvent::RpcFailed(_)) => {
            (MenuState::Closed, Vec::new())
        }
        (_, MenuEvent::RpcDone(outcome)) => (result(outcome_message(outcome)), Vec::new()),
        (_, MenuEvent::RpcFailed(message)) => (result(format!("Error: {message}")), Vec::new()),
        (MenuState::Closed, _) => (MenuState::Closed, Vec::new()),
        (MenuState::Root { selected }, MenuEvent::Key(key)) => root_step(selected, key),
        (MenuState::ConfirmKill, MenuEvent::Key(key)) => confirm_kill_step(key),
        (MenuState::RenameInput { buffer }, MenuEvent::Key(key)) => rename_step(buffer, key),
        (MenuState::Busy { label }, MenuEvent::Key(key)) => busy_key_step(label, key),
        (MenuState::Result { .. }, MenuEvent::Key(MenuKey::Esc)) => {
            (MenuState::open_root(), Vec::new())
        }
        (state, _) => (state, Vec::new()),
    }
}

fn root_step(selected: usize, key: MenuKey) -> (MenuState, Vec<MenuEffect>) {
    match key {
        MenuKey::Byte(b'j') | MenuKey::Down => (
            MenuState::Root {
                selected: next_index(selected),
            },
            Vec::new(),
        ),
        MenuKey::Up => (
            MenuState::Root {
                selected: previous_index(selected),
            },
            Vec::new(),
        ),
        MenuKey::Byte(b'k') => (MenuState::ConfirmKill, Vec::new()),
        MenuKey::Byte(b'd') => (
            MenuState::Closed,
            vec![MenuEffect::RunDetach, MenuEffect::Close],
        ),
        MenuKey::Byte(b'n') => (busy("Starting session"), vec![MenuEffect::RunNewSession]),
        MenuKey::Byte(b'f') => (busy("Forking session"), vec![MenuEffect::RunFork]),
        MenuKey::Byte(b'r') => (
            MenuState::RenameInput {
                buffer: String::new(),
            },
            Vec::new(),
        ),
        MenuKey::Enter => run_selected(selected),
        MenuKey::Esc => (MenuState::Closed, vec![MenuEffect::Close]),
        MenuKey::Mouse | MenuKey::Backspace | MenuKey::Byte(_) => {
            (MenuState::Root { selected }, Vec::new())
        }
    }
}

fn confirm_kill_step(key: MenuKey) -> (MenuState, Vec<MenuEffect>) {
    match key {
        MenuKey::Byte(b'y' | b'Y') => (busy("Killing session"), vec![MenuEffect::RunKill]),
        MenuKey::Esc | MenuKey::Byte(b'n' | b'N') => (MenuState::open_root(), Vec::new()),
        _ => (MenuState::ConfirmKill, Vec::new()),
    }
}

fn rename_step(mut buffer: String, key: MenuKey) -> (MenuState, Vec<MenuEffect>) {
    match key {
        MenuKey::Esc => (MenuState::open_root(), Vec::new()),
        MenuKey::Enter => {
            let name = buffer;
            (busy("Renaming session"), vec![MenuEffect::RunRename(name)])
        }
        MenuKey::Backspace => {
            buffer.pop();
            (MenuState::RenameInput { buffer }, Vec::new())
        }
        MenuKey::Byte(byte) if is_rename_text_byte(byte) => {
            if buffer.len() < MAX_SESSION_NAME_BYTES {
                buffer.push(char::from(byte));
            }
            (MenuState::RenameInput { buffer }, Vec::new())
        }
        _ => (MenuState::RenameInput { buffer }, Vec::new()),
    }
}

fn busy_key_step(label: String, key: MenuKey) -> (MenuState, Vec<MenuEffect>) {
    if key == MenuKey::Esc {
        (MenuState::Closed, vec![MenuEffect::Close])
    } else {
        (MenuState::Busy { label }, Vec::new())
    }
}

fn run_selected(selected: usize) -> (MenuState, Vec<MenuEffect>) {
    match selected {
        ROOT_KILL_INDEX => (MenuState::ConfirmKill, Vec::new()),
        ROOT_DETACH_INDEX => (
            MenuState::Closed,
            vec![MenuEffect::RunDetach, MenuEffect::Close],
        ),
        ROOT_NEW_SESSION_INDEX => (busy("Starting session"), vec![MenuEffect::RunNewSession]),
        ROOT_FORK_INDEX => (busy("Forking session"), vec![MenuEffect::RunFork]),
        ROOT_RENAME_INDEX => (
            MenuState::RenameInput {
                buffer: String::new(),
            },
            Vec::new(),
        ),
        _ => (MenuState::open_root(), Vec::new()),
    }
}

fn next_index(selected: usize) -> usize {
    (selected + 1) % ROOT_ITEM_COUNT
}

fn previous_index(selected: usize) -> usize {
    if selected == ROOT_DEFAULT_SELECTED {
        ROOT_ITEM_COUNT - 1
    } else {
        selected - 1
    }
}

fn busy(label: &str) -> MenuState {
    MenuState::Busy {
        label: label.to_owned(),
    }
}

fn result(message: String) -> MenuState {
    MenuState::Result { message }
}

fn outcome_message(outcome: MenuOutcome) -> String {
    match outcome {
        MenuOutcome::Killed => "Session stopped".to_owned(),
        MenuOutcome::NewSession { id } => format!("New session created: {id}"),
        MenuOutcome::Forked { id } => format!("Forked session created: {id}"),
        MenuOutcome::Renamed { name: Some(name) } => format!("Session renamed: {name}"),
        MenuOutcome::Renamed { name: None } => "Session name cleared".to_owned(),
    }
}

fn is_rename_text_byte(byte: u8) -> bool {
    byte.is_ascii_graphic() || byte == b' '
}

fn root_overlay(selected: usize) -> OverlayFrame {
    OverlayFrame {
        title: "Session menu".to_owned(),
        lines: vec![
            overlay_line("k  Kill session", selected == ROOT_KILL_INDEX),
            overlay_line("d  Detach", selected == ROOT_DETACH_INDEX),
            overlay_line(
                "n  New session in this worktree",
                selected == ROOT_NEW_SESSION_INDEX,
            ),
            overlay_line("f  Fork session", selected == ROOT_FORK_INDEX),
            overlay_line("r  Rename session", selected == ROOT_RENAME_INDEX),
        ],
        footer: Some("Enter select  Esc close".to_owned()),
        cursor: None,
    }
}

fn confirm_kill_overlay() -> OverlayFrame {
    OverlayFrame {
        title: "Kill session?".to_owned(),
        lines: vec![
            overlay_line("This stops the PTY process.", false),
            overlay_line("y  Confirm kill", true),
        ],
        footer: Some("Esc/n back".to_owned()),
        cursor: None,
    }
}

fn rename_overlay(buffer: &str) -> OverlayFrame {
    let cursor_col = RENAME_INPUT_PREFIX.chars().count() + buffer.chars().count();
    OverlayFrame {
        title: "Rename session".to_owned(),
        lines: vec![overlay_line(
            &format!("{RENAME_INPUT_PREFIX}{buffer}"),
            true,
        )],
        footer: Some("Enter save  Esc back".to_owned()),
        cursor: Some((
            RENAME_INPUT_CURSOR_ROW,
            cursor_col.try_into().unwrap_or(u16::MAX),
        )),
    }
}

fn busy_overlay(label: &str) -> OverlayFrame {
    OverlayFrame {
        title: "Working".to_owned(),
        lines: vec![overlay_line(label, false)],
        footer: Some("Esc close".to_owned()),
        cursor: None,
    }
}

fn result_overlay(message: &str) -> OverlayFrame {
    OverlayFrame {
        title: "Result".to_owned(),
        lines: vec![overlay_line(message, false)],
        footer: Some("Esc back".to_owned()),
        cursor: None,
    }
}

fn overlay_line(text: &str, highlighted: bool) -> OverlayLine {
    OverlayLine {
        text: text.to_owned(),
        highlighted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RENAME_TEST_BYTE: u8 = b'a';

    #[test]
    fn rename_input_ignores_graphic_bytes_after_session_name_limit() {
        let mut state = MenuState::RenameInput {
            buffer: String::new(),
        };

        for _ in 0..=MAX_SESSION_NAME_BYTES {
            let (next, effects) = step(state, MenuEvent::Key(MenuKey::Byte(RENAME_TEST_BYTE)));
            state = next;
            assert!(effects.is_empty());
        }

        let MenuState::RenameInput { buffer } = state else {
            panic!("rename input should remain open");
        };
        assert_eq!(buffer.len(), MAX_SESSION_NAME_BYTES);
    }
}
