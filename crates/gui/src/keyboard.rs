//! Keyboard shortcuts (UX spec section D, phases B4/B5).
//!
//! Iced offers no direct "is any widget focused" query, so the docs for
//! `keyboard::listen()` warn that a subscription sees *every* key press. In
//! practice this is not quite true: `iced_widget`'s `text_input`/`text_editor`
//! call `shell.capture_event()` for the key presses they handle while
//! focused (typed characters, Backspace, arrows, Home/End, and Enter when an
//! `on_submit` is set — see e.g. `text_input::update`'s `state.is_focused`
//! guard), and `keyboard::listen()` only forwards events left `Ignored` by
//! every widget. So a key typed into a focused field never reaches
//! [`route_key_press`] in the first place; that is the focus guard, gotten
//! for free from Iced's own event-capture semantics rather than a hand-rolled
//! "am I focused" flag that could drift out of sync.
//!
//! One corollary: `Escape` unfocuses a focused text field on its own (without
//! closing anything), consuming that first press; a *second* `Escape` then
//! reaches us with nothing focused and closes/backs the modal. Two-step, but
//! predictable, and it means Escape can never accidentally discard a field's
//! edit-in-progress against the operator's expectation.
//!
//! A modal being open is a *separate* gate from focus: a modal can be open
//! with nothing focused inside it yet, so [`route_key_press`] checks
//! `app.modal` directly rather than relying on capture semantics for that
//! part.
//!
//! Subscription closures must be non-capturing (Iced asserts they are
//! zero-sized), so the raw key event is wrapped in `Message::KeyPressed` and
//! all the app-state-dependent routing happens in [`route_key_press`], called
//! from `command::update` where `&PohunekApp` is available.

use iced::keyboard::key::Named;
use iced::keyboard::{self, Key, Modifiers};
use iced::Subscription;
use pohunek_gui_core::{ProviderPanel, RightTab};

use crate::message::{InboxView, Message, ModalView};
use crate::selection::{selected_host_id, selected_session, tab_project_scope};
use crate::view::provider::{selected_linear_issue_in_state, selected_pull_request_in_state};
use crate::PohunekApp;

/// Subscribes to every key press Iced did not already hand to a focused
/// widget (see the module docs for what that excludes).
pub(crate) fn subscription() -> Subscription<Message> {
    keyboard::listen().filter_map(keyboard_event_to_message)
}

/// Keeps only key-press events; releases and bare modifier changes carry no
/// shortcut meaning here.
fn keyboard_event_to_message(event: keyboard::Event) -> Option<Message> {
    match event {
        keyboard::Event::KeyPressed { key, modifiers, .. } => {
            Some(Message::KeyPressed { key, modifiers })
        }
        keyboard::Event::KeyReleased { .. } | keyboard::Event::ModifiersChanged(_) => None,
    }
}

/// Routes one key press to zero or more existing messages, replaying the same
/// reducer path a click on the equivalent button would take.
pub(crate) fn route_key_press(app: &PohunekApp, key: &Key, modifiers: Modifiers) -> Vec<Message> {
    if app.modal == ModalView::None {
        route_global_key(app, key, modifiers)
    } else {
        route_modal_key(app, key, modifiers)
    }
}

/// Global shortcuts, active only while no modal is open (a modal has its own,
/// smaller key set — see [`route_modal_key`]).
fn route_global_key(app: &PohunekApp, key: &Key, modifiers: Modifiers) -> Vec<Message> {
    // Require a bare key: Ctrl/Alt/Logo combos are left to the OS or desktop
    // environment, and Shift would remap digits to punctuation on most
    // layouts anyway, so it can never match the character arms below.
    if !modifiers.is_empty() {
        return Vec::new();
    }
    match key.as_ref() {
        Key::Character("1") => vec![Message::SelectTab(RightTab::Detail)],
        Key::Character("2") => tab_shortcut(app, RightTab::Linear),
        Key::Character("3") => tab_shortcut(app, RightTab::GitHub),
        Key::Character("4") => tab_shortcut(app, RightTab::Worktrees),
        Key::Character("i") => vec![Message::OpenInbox],
        Key::Character("b") => vec![Message::CycleBlockedAgent],
        Key::Character("o") => open_selected_session(app),
        Key::Character("n") => vec![Message::OpenStartModal],
        Key::Character("a") => vec![Message::OpenAssistantModal],
        Key::Character("r") => refresh_active_tab(app),
        _ => Vec::new(),
    }
}

/// A tab-switch shortcut, gated exactly like `view::detail::tab_button`'s
/// disabled state: a tab with no project scope has no `on_press`, so its
/// digit shortcut is a no-op too instead of jumping to an empty placeholder.
fn tab_shortcut(app: &PohunekApp, tab: RightTab) -> Vec<Message> {
    if tab_project_scope(app).is_some() {
        vec![Message::SelectTab(tab)]
    } else {
        Vec::new()
    }
}

/// `o`: open the currently selected session in a terminal, mirroring the
/// session pane's "Open in terminal" button. No-op without a session selected.
fn open_selected_session(app: &PohunekApp) -> Vec<Message> {
    let Some((host_id, session)) = selected_session(app) else {
        return Vec::new();
    };
    vec![Message::OpenSession {
        host_id: host_id.clone(),
        session_id: session.id.clone(),
    }]
}

/// `r`: refresh whichever tab is active, mirroring that tab's own Fetch/
/// Refresh button. No-op on a tab with no project scope, matching the tab
/// bodies' own "select a project" empty state instead of erroring.
fn refresh_active_tab(app: &PohunekApp) -> Vec<Message> {
    if tab_project_scope(app).is_none() {
        return Vec::new();
    }
    match app.ui_state.active_tab {
        RightTab::Detail | RightTab::Worktrees => vec![Message::ShowProject],
        RightTab::Linear => vec![Message::FetchLinearIssues],
        RightTab::GitHub => vec![Message::FetchGitHubPullRequests, Message::FetchGitHubIssues],
    }
}

/// Modal-scoped shortcuts: only `Esc` (back/close) and `Enter` (primary
/// action) apply while a modal is open; everything else falls through to
/// whatever the focused widget wants to do with it.
fn route_modal_key(app: &PohunekApp, key: &Key, modifiers: Modifiers) -> Vec<Message> {
    match key.as_ref() {
        Key::Named(Named::Escape) => vec![escape_message(app)],
        Key::Named(Named::Enter) => enter_messages(app, modifiers),
        _ => Vec::new(),
    }
}

/// `Esc`: step the inbox message layer back to the list, else close the modal.
fn escape_message(app: &PohunekApp) -> Message {
    if app.modal == ModalView::Inbox && matches!(app.inbox_view, InboxView::Message { .. }) {
        Message::InboxBack
    } else {
        Message::CloseModal
    }
}

/// `Enter`: the modal's primary action, matching whichever button its content
/// renders as `button::primary` (Launch / Start / Open session).
fn enter_messages(app: &PohunekApp, modifiers: Modifiers) -> Vec<Message> {
    match app.modal {
        ModalView::Start => vec![Message::CreateSession],
        ModalView::Assistant => vec![Message::LaunchAssistant],
        ModalView::ProviderItem => provider_item_enter(app),
        ModalView::Inbox => inbox_enter(app, modifiers),
        // `route_key_press` only calls into modal-scoped routing when a modal
        // is open; kept explicit (no wildcard) so a future `ModalView`
        // variant fails to compile here instead of silently doing nothing.
        ModalView::None => Vec::new(),
    }
}

/// The provider item modal's `Enter`: Launch, whichever provider is showing —
/// matching `view::modals::linear_issue_modal`/`github_pull_request_modal`,
/// the only two with a Launch button. A GitHub issue modal has none (it is
/// reference-only), and an empty Linear panel shows its "No issue selected"
/// guard instead of a button, so both fall through to no-op.
fn provider_item_enter(app: &PohunekApp) -> Vec<Message> {
    let Ok(host_id) = selected_host_id(app) else {
        return Vec::new();
    };
    let Some(host) = app.workspace.hosts.get(&host_id) else {
        return Vec::new();
    };
    match host.provider.active_panel {
        ProviderPanel::Linear
            if selected_linear_issue_in_state(&host.provider.linear).is_some() =>
        {
            vec![Message::LaunchLinearIssue]
        }
        ProviderPanel::GitHub
            if selected_pull_request_in_state(&host.provider.github).is_some() =>
        {
            vec![Message::LaunchGitHubPullRequest]
        }
        ProviderPanel::Linear | ProviderPanel::GitHub => Vec::new(),
    }
}

/// The inbox modal's `Enter`/`Shift+Enter`, message layer only: `Enter` opens
/// the linked session in place (`Message::OpenNotificationLink`, same as the
/// primary button); `Shift+Enter` additionally opens it in a terminal. Both
/// no-op when the notification has no session or its session is no longer
/// live, matching `view::inbox::notification_link_action`'s own guard (no
/// button shown there either).
fn inbox_enter(app: &PohunekApp, modifiers: Modifiers) -> Vec<Message> {
    let InboxView::Message {
        host_id,
        notification_id,
    } = &app.inbox_view
    else {
        // The list layer has no notion of a "highlighted" row yet — that
        // needs the phase-2 (B5) j/k list-navigation cursor.
        // TODO(B5): once a list cursor exists, Enter here should open its
        // row, the same as clicking it.
        return Vec::new();
    };
    let Some(record) = app.workspace.notification(host_id, notification_id) else {
        return Vec::new();
    };
    let Some(session_id) = record.session_id.clone() else {
        return Vec::new();
    };
    let live = app
        .workspace
        .hosts
        .get(host_id)
        .is_some_and(|host| host.sessions.contains_key(&session_id.0));
    if !live {
        return Vec::new();
    }
    let mut messages = vec![Message::OpenNotificationLink {
        host_id: host_id.clone(),
        notification_id: notification_id.clone(),
    }];
    if modifiers.shift() {
        messages.push(Message::OpenSession {
            host_id: host_id.clone(),
            session_id,
        });
    }
    messages
}
