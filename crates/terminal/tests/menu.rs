use pohunek_terminal::{
    step, MenuEffect, MenuEvent, MenuKey, MenuOutcome, MenuState, OverlayFrame, OverlayLine,
};

#[test]
fn root_navigation_wraps_and_enter_runs_selected_action() {
    let (state, effects) = step(MenuState::open_root(), MenuEvent::Key(MenuKey::Down));
    assert_eq!(state, MenuState::Root { selected: 1 });
    assert!(effects.is_empty());

    let (state, effects) = step(state, MenuEvent::Key(MenuKey::Up));
    assert_eq!(state, MenuState::Root { selected: 0 });
    assert!(effects.is_empty());

    let (state, effects) = step(state, MenuEvent::Key(MenuKey::Up));
    assert_eq!(state, MenuState::Root { selected: 4 });
    assert!(effects.is_empty());

    let (state, effects) = step(state, MenuEvent::Key(MenuKey::Enter));
    assert_eq!(
        state,
        MenuState::RenameInput {
            buffer: String::new()
        }
    );
    assert!(effects.is_empty());
}

#[test]
fn direct_hotkeys_run_phase_one_actions() {
    let (state, effects) = step(MenuState::open_root(), MenuEvent::Key(MenuKey::Byte(b'd')));
    assert_eq!(state, MenuState::Closed);
    assert_eq!(effects, vec![MenuEffect::RunDetach, MenuEffect::Close]);

    let (state, effects) = step(MenuState::open_root(), MenuEvent::Key(MenuKey::Byte(b'n')));
    assert_eq!(
        state,
        MenuState::Busy {
            label: "Starting session".to_owned()
        }
    );
    assert_eq!(effects, vec![MenuEffect::RunNewSession]);

    let (state, effects) = step(MenuState::open_root(), MenuEvent::Key(MenuKey::Byte(b'f')));
    assert_eq!(
        state,
        MenuState::Busy {
            label: "Forking session".to_owned()
        }
    );
    assert_eq!(effects, vec![MenuEffect::RunFork]);

    let (state, effects) = step(MenuState::open_root(), MenuEvent::Key(MenuKey::Byte(b'r')));
    assert_eq!(
        state,
        MenuState::RenameInput {
            buffer: String::new()
        }
    );
    assert!(effects.is_empty());
}

#[test]
fn escape_walks_back_through_modal_states() {
    let (state, effects) = step(
        MenuState::Result {
            message: "created s-9".to_owned(),
        },
        MenuEvent::Key(MenuKey::Esc),
    );
    assert_eq!(state, MenuState::Root { selected: 0 });
    assert!(effects.is_empty());

    let (state, effects) = step(state, MenuEvent::Key(MenuKey::Esc));
    assert_eq!(state, MenuState::Closed);
    assert_eq!(effects, vec![MenuEffect::Close]);
}

#[test]
fn confirm_kill_requires_y_before_running_stop() {
    let (state, effects) = step(MenuState::open_root(), MenuEvent::Key(MenuKey::Byte(b'k')));
    assert_eq!(state, MenuState::ConfirmKill);
    assert!(effects.is_empty());

    let (state, effects) = step(state, MenuEvent::Key(MenuKey::Byte(b'n')));
    assert_eq!(state, MenuState::Root { selected: 0 });
    assert!(effects.is_empty());

    let (state, _) = step(MenuState::open_root(), MenuEvent::Key(MenuKey::Byte(b'k')));
    let (state, effects) = step(state, MenuEvent::Key(MenuKey::Byte(b'y')));
    assert_eq!(
        state,
        MenuState::Busy {
            label: "Killing session".to_owned()
        }
    );
    assert_eq!(effects, vec![MenuEffect::RunKill]);
}

#[test]
fn rename_input_edits_buffer_and_submits() {
    let mut state = MenuState::RenameInput {
        buffer: String::new(),
    };
    for byte in b"new name" {
        let (next, effects) = step(state, MenuEvent::Key(MenuKey::Byte(*byte)));
        state = next;
        assert!(effects.is_empty());
    }

    let (state, effects) = step(state, MenuEvent::Key(MenuKey::Backspace));
    assert_eq!(
        state,
        MenuState::RenameInput {
            buffer: "new nam".to_owned()
        }
    );
    assert!(effects.is_empty());

    let (state, effects) = step(state, MenuEvent::Key(MenuKey::Enter));
    assert_eq!(
        state,
        MenuState::Busy {
            label: "Renaming session".to_owned()
        }
    );
    assert_eq!(effects, vec![MenuEffect::RunRename("new nam".to_owned())]);
}

#[test]
fn busy_ignores_input_except_escape_and_drops_late_rpc_results_when_closed() {
    let state = MenuState::Busy {
        label: "Starting session".to_owned(),
    };

    let (state, effects) = step(state, MenuEvent::Key(MenuKey::Byte(b'r')));
    assert_eq!(
        state,
        MenuState::Busy {
            label: "Starting session".to_owned()
        }
    );
    assert!(effects.is_empty());

    let (state, effects) = step(state, MenuEvent::Key(MenuKey::Esc));
    assert_eq!(state, MenuState::Closed);
    assert_eq!(effects, vec![MenuEffect::Close]);

    let (state, effects) = step(
        state,
        MenuEvent::RpcDone(MenuOutcome::NewSession {
            id: "s-10".to_owned(),
        }),
    );
    assert_eq!(state, MenuState::Closed);
    assert!(effects.is_empty());
}

#[test]
fn rpc_done_and_failed_from_busy_render_result_messages() {
    let busy = MenuState::Busy {
        label: "Starting session".to_owned(),
    };
    let (state, effects) = step(
        busy,
        MenuEvent::RpcDone(MenuOutcome::NewSession {
            id: "s-10".to_owned(),
        }),
    );
    assert_eq!(
        state,
        MenuState::Result {
            message: "New session created: s-10".to_owned()
        }
    );
    assert!(effects.is_empty());

    let busy = MenuState::Busy {
        label: "Forking session".to_owned(),
    };
    let (state, effects) = step(
        busy,
        MenuEvent::RpcDone(MenuOutcome::Forked {
            id: "s-11".to_owned(),
        }),
    );
    assert_eq!(
        state,
        MenuState::Result {
            message: "Forked session created: s-11".to_owned()
        }
    );
    assert!(effects.is_empty());

    let busy = MenuState::Busy {
        label: "Renaming session".to_owned(),
    };
    let (state, effects) = step(busy, MenuEvent::RpcFailed("name rejected".to_owned()));
    assert_eq!(
        state,
        MenuState::Result {
            message: "Error: name rejected".to_owned()
        }
    );
    assert!(effects.is_empty());
}

#[test]
fn overlay_frame_for_root_marks_selected_item() {
    let frame = MenuState::Root { selected: 2 }
        .to_overlay_frame()
        .expect("root menu renders overlay");

    assert_eq!(frame.title, "Session menu");
    assert_eq!(
        frame.lines,
        vec![
            OverlayLine {
                text: "k  Kill session".to_owned(),
                highlighted: false
            },
            OverlayLine {
                text: "d  Detach".to_owned(),
                highlighted: false
            },
            OverlayLine {
                text: "n  New session in this worktree".to_owned(),
                highlighted: true
            },
            OverlayLine {
                text: "f  Fork session".to_owned(),
                highlighted: false
            },
            OverlayLine {
                text: "r  Rename session".to_owned(),
                highlighted: false
            },
        ]
    );
    assert_eq!(frame.footer.as_deref(), Some("Enter select  Esc close"));
    assert_eq!(frame.cursor, None);
}

#[test]
fn overlay_frame_for_rename_places_cursor_after_input_prefix() {
    let frame = MenuState::RenameInput {
        buffer: "abc".to_owned(),
    }
    .to_overlay_frame()
    .expect("rename input renders overlay");

    assert_eq!(
        frame,
        OverlayFrame {
            title: "Rename session".to_owned(),
            lines: vec![OverlayLine {
                text: "Name: abc".to_owned(),
                highlighted: true
            }],
            footer: Some("Enter save  Esc back".to_owned()),
            cursor: Some((1, 9))
        }
    );
}

#[test]
fn closed_state_has_no_overlay_frame() {
    assert_eq!(MenuState::Closed.to_overlay_frame(), None);
}
