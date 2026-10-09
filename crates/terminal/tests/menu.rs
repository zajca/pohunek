//! Menu state and terminal composition are exercised as one attach interaction.

use pohunek_terminal::{step, Compositor, MenuEffect, MenuEvent, MenuKey, MenuOutcome, MenuState};

const TERMINAL_COLS: u16 = 120;
const TERMINAL_ROWS: u16 = 24;
const EXPECTED_SESSION_NAME_LIMIT_BYTES: usize = 128;

fn rendered_menu(state: &MenuState) -> String {
    let mut compositor = Compositor::new(TERMINAL_COLS, TERMINAL_ROWS);
    compositor.feed(b"agent prompt");
    compositor.set_overlay(state.to_overlay_frame());
    let mut terminal = vt100::Parser::new(TERMINAL_ROWS, TERMINAL_COLS, 0);
    terminal.process(&compositor.render());
    terminal.screen().contents()
}

#[test]
fn navigation_and_session_actions_render_their_progress_and_results() {
    let root = MenuState::open_root();
    let screen = rendered_menu(&root);
    for label in [
        "Session menu",
        "Kill session",
        "Terminate and delete session",
        "Detach",
        "New session in this worktree",
        "Fork session",
        "Rename session",
        "Enter select  Esc close",
    ] {
        assert!(screen.contains(label), "menu omitted {label:?}");
    }

    let (down, effects) = step(root, MenuEvent::Key(MenuKey::Down));
    assert_eq!(down, MenuState::Root { selected: 1 });
    assert!(effects.is_empty());
    let (remove_prompt, effects) = step(down, MenuEvent::Key(MenuKey::Enter));
    assert_eq!(remove_prompt, MenuState::ConfirmRemove);
    assert!(effects.is_empty());
    assert!(rendered_menu(&remove_prompt).contains("Terminate and delete"));
    let (root, effects) = step(remove_prompt, MenuEvent::Key(MenuKey::Esc));
    assert_eq!(root, MenuState::open_root());
    assert!(effects.is_empty());

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
        assert!(rendered_menu(&busy).contains(busy_text));
        let (result_state, effects) = step(busy, MenuEvent::RpcDone(result));
        assert!(effects.is_empty());
        assert!(rendered_menu(&result_state).contains(result_text));
        let (returned, effects) = step(result_state, MenuEvent::Key(MenuKey::Esc));
        assert_eq!(returned, root);
        assert!(effects.is_empty());
    }

    let (closed, effects) = step(root, MenuEvent::Key(MenuKey::Byte(b'd')));
    assert_eq!(effects, vec![MenuEffect::RunDetach, MenuEffect::Close]);
    assert_eq!(closed, MenuState::Closed);
    let screen = rendered_menu(&closed);
    assert!(screen.contains("agent prompt"));
    assert!(!screen.contains("Session menu"));
}

#[test]
fn destructive_actions_require_confirmation_before_the_menu_runs_them() {
    let (kill_prompt, effects) = step(MenuState::open_root(), MenuEvent::Key(MenuKey::Byte(b'k')));
    assert_eq!(kill_prompt, MenuState::ConfirmKill);
    assert!(effects.is_empty());
    assert!(rendered_menu(&kill_prompt).contains("Kill session"));
    let (root, effects) = step(kill_prompt, MenuEvent::Key(MenuKey::Byte(b'n')));
    assert_eq!(root, MenuState::open_root());
    assert!(effects.is_empty());
    let (kill_prompt, _) = step(root, MenuEvent::Key(MenuKey::Byte(b'k')));
    let (busy, effects) = step(kill_prompt, MenuEvent::Key(MenuKey::Byte(b'y')));
    assert_eq!(effects, vec![MenuEffect::RunKill]);
    assert!(rendered_menu(&busy).contains("Killing session"));
    let (result, effects) = step(busy, MenuEvent::RpcDone(MenuOutcome::Killed));
    assert!(effects.is_empty());
    assert!(rendered_menu(&result).contains("Session stopped"));

    let (root, _) = step(result, MenuEvent::Key(MenuKey::Esc));
    let (remove_prompt, effects) = step(root, MenuEvent::Key(MenuKey::Byte(b't')));
    assert_eq!(remove_prompt, MenuState::ConfirmRemove);
    assert!(effects.is_empty());
    assert!(rendered_menu(&remove_prompt).contains("Terminate and delete"));
    let (still_prompt, effects) = step(remove_prompt, MenuEvent::Key(MenuKey::Byte(b'x')));
    assert_eq!(still_prompt, MenuState::ConfirmRemove);
    assert!(effects.is_empty());
    let (busy, effects) = step(still_prompt, MenuEvent::Key(MenuKey::Byte(b'y')));
    assert_eq!(effects, vec![MenuEffect::RunRemove]);
    assert!(rendered_menu(&busy).contains("Terminating and deleting session"));
    let (result, effects) = step(
        busy,
        MenuEvent::RpcDone(MenuOutcome::Removed {
            worktrees_removed: 1,
            worktrees_failed: 2,
        }),
    );
    assert!(effects.is_empty());
    assert!(rendered_menu(&result).contains("worktrees left on disk: 2"));
}

#[test]
fn rename_input_is_bounded_and_the_composed_result_reports_failure() {
    let (last_row, effects) = step(MenuState::open_root(), MenuEvent::Key(MenuKey::Up));
    assert_eq!(last_row, MenuState::Root { selected: 5 });
    assert!(effects.is_empty());
    let (mut input, effects) = step(last_row, MenuEvent::Key(MenuKey::Enter));
    assert!(effects.is_empty());
    assert!(rendered_menu(&input).contains("Rename session"));
    for byte in b"new name" {
        let (next, effects) = step(input, MenuEvent::Key(MenuKey::Byte(*byte)));
        input = next;
        assert!(effects.is_empty());
    }
    let (input, effects) = step(input, MenuEvent::Key(MenuKey::Backspace));
    assert!(effects.is_empty());
    assert!(rendered_menu(&input).contains("Name: new nam"));
    let (busy, effects) = step(input, MenuEvent::Key(MenuKey::Enter));
    assert_eq!(effects, vec![MenuEffect::RunRename("new nam".to_owned())]);
    assert!(rendered_menu(&busy).contains("Renaming session"));
    let (result, effects) = step(busy, MenuEvent::RpcFailed("name rejected".to_owned()));
    assert!(effects.is_empty());
    assert!(rendered_menu(&result).contains("Error: name rejected"));

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
    assert!(rendered_menu(&input).contains("Name: a"));
}

#[test]
fn closing_a_busy_menu_restores_the_agent_screen_and_ignores_late_results() {
    let (busy, effects) = step(MenuState::open_root(), MenuEvent::Key(MenuKey::Byte(b'n')));
    assert_eq!(effects, vec![MenuEffect::RunNewSession]);
    let (busy, effects) = step(busy, MenuEvent::Key(MenuKey::Byte(b'r')));
    assert!(effects.is_empty());
    assert!(rendered_menu(&busy).contains("Starting session"));
    let (closed, effects) = step(busy, MenuEvent::Key(MenuKey::Esc));
    assert_eq!(effects, vec![MenuEffect::Close]);
    assert_eq!(closed, MenuState::Closed);
    let (closed, effects) = step(
        closed,
        MenuEvent::RpcDone(MenuOutcome::NewSession {
            id: "s-10".to_owned(),
        }),
    );
    assert!(effects.is_empty());
    let screen = rendered_menu(&closed);
    assert!(screen.contains("agent prompt"));
    assert!(!screen.contains("New session created"));
}
