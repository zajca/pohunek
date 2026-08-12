//! Keyboard shortcuts for the session-first native GUI.
//!
//! Focused Iced inputs consume handled key presses before the global
//! subscription sees them. Modal routing is therefore gated by `ModalView`,
//! while input editing remains owned by each widget.

// Rust guideline compliant 2026-08-12

use std::collections::BTreeMap;
use std::fmt;

use iced::keyboard::key::Named;
use iced::keyboard::{self, Key, Modifiers};
use iced::widget::{operation, Id};
use iced::{Subscription, Task};

use crate::message::{InboxView, ListDirection, Message, ModalView};
use crate::selection::{selected_project, selected_session};
use crate::PohunekApp;

/// Keyboard routing scope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeyContext {
    Global,
    Modal,
}

impl fmt::Display for KeyContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Global => f.write_str("global"),
            Self::Modal => f.write_str("modal"),
        }
    }
}

/// Shortcut action resolved from a key chord.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeyAction {
    OpenInbox,
    OpenSelectedSession,
    ShowSelectedSession,
    OpenKeymapHelp,
    NewSession,
    OpenAssistant,
    ModalBack,
    ModalPrimary,
    ModalPrimaryWithTerminal,
    OpenLinkedSession,
    ListUp,
    ListDown,
}

const START_NAME_INPUT_ID: &str = "start-session-name";
const START_PROMPT_INPUT_ID: &str = "start-session-prompt";
const START_BRANCH_INPUT_ID: &str = "start-session-branch";
const START_BASE_BRANCH_INPUT_ID: &str = "start-session-base-branch";
const ASSISTANT_REQUEST_INPUT_ID: &str = "assistant-request";
const ASSISTANT_BRANCH_INPUT_ID: &str = "assistant-branch";
const ASSISTANT_BASE_BRANCH_INPUT_ID: &str = "assistant-base-branch";
const READ_ONLY_TEXT_INPUT_ID: &str = "read-only-selectable-text";

pub(crate) fn start_name_input_id() -> Id {
    Id::new(START_NAME_INPUT_ID)
}

pub(crate) fn start_prompt_input_id() -> Id {
    Id::new(START_PROMPT_INPUT_ID)
}

pub(crate) fn start_branch_input_id() -> Id {
    Id::new(START_BRANCH_INPUT_ID)
}

pub(crate) fn start_base_branch_input_id() -> Id {
    Id::new(START_BASE_BRANCH_INPUT_ID)
}

pub(crate) fn assistant_request_input_id() -> Id {
    Id::new(ASSISTANT_REQUEST_INPUT_ID)
}

pub(crate) fn assistant_branch_input_id() -> Id {
    Id::new(ASSISTANT_BRANCH_INPUT_ID)
}

pub(crate) fn assistant_base_branch_input_id() -> Id {
    Id::new(ASSISTANT_BASE_BRANCH_INPUT_ID)
}

pub(crate) fn read_only_text_input_id() -> Id {
    Id::new(READ_ONLY_TEXT_INPUT_ID)
}

/// Config error raised while building a keymap from `gui.toml`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum KeyMapError {
    UnknownBinding {
        name: String,
    },
    InvalidKey {
        binding: String,
        value: String,
        reason: String,
    },
    Conflict {
        context: KeyContext,
        chord: String,
        first: &'static str,
        second: &'static str,
    },
}

impl fmt::Display for KeyMapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownBinding { name } => write!(f, "unknown keybinding `{name}`"),
            Self::InvalidKey {
                binding,
                value,
                reason,
            } => write!(f, "invalid key `{value}` for `{binding}`: {reason}"),
            Self::Conflict {
                context,
                chord,
                first,
                second,
            } => write!(
                f,
                "key `{chord}` in {context} context is bound to both `{first}` and `{second}`"
            ),
        }
    }
}

impl std::error::Error for KeyMapError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BindingId {
    OpenInbox,
    OpenSelectedSession,
    ShowSelectedSession,
    OpenKeymapHelp,
    NewSession,
    OpenAssistant,
    ModalBack,
    ModalPrimary,
    ModalPrimaryWithTerminal,
    ModalListUp,
    ModalListUpArrow,
    ModalListDown,
    ModalListDownArrow,
    ModalOpenLinkedSession,
}

impl BindingId {
    const ALL: &'static [Self] = &[
        Self::OpenInbox,
        Self::OpenSelectedSession,
        Self::ShowSelectedSession,
        Self::OpenKeymapHelp,
        Self::NewSession,
        Self::OpenAssistant,
        Self::ModalBack,
        Self::ModalPrimary,
        Self::ModalPrimaryWithTerminal,
        Self::ModalListUp,
        Self::ModalListUpArrow,
        Self::ModalListDown,
        Self::ModalListDownArrow,
        Self::ModalOpenLinkedSession,
    ];

    fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|binding| binding.config_name() == value)
    }

    const fn config_name(self) -> &'static str {
        match self {
            Self::OpenInbox => "open_inbox",
            Self::OpenSelectedSession => "open_selected_session",
            Self::ShowSelectedSession => "show_selected_session",
            Self::OpenKeymapHelp => "open_keymap_help",
            Self::NewSession => "new_session",
            Self::OpenAssistant => "open_assistant",
            Self::ModalBack => "modal_back",
            Self::ModalPrimary => "modal_primary",
            Self::ModalPrimaryWithTerminal => "modal_primary_with_terminal",
            Self::ModalListUp => "modal_list_up",
            Self::ModalListUpArrow => "modal_list_up_arrow",
            Self::ModalListDown => "modal_list_down",
            Self::ModalListDownArrow => "modal_list_down_arrow",
            Self::ModalOpenLinkedSession => "modal_open_linked_session",
        }
    }
}

/// A normalized key chord used by the shortcut map.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct KeyChord {
    key: ChordKey,
    modifiers: ChordModifiers,
}

impl KeyChord {
    pub(crate) fn character(value: &str) -> Self {
        Self {
            key: ChordKey::Character(value.to_lowercase()),
            modifiers: ChordModifiers::empty(),
        }
    }

    pub(crate) fn named(value: Named) -> Self {
        Self {
            key: ChordKey::Named(value),
            modifiers: ChordModifiers::empty(),
        }
    }

    fn shift_named(value: Named) -> Self {
        Self {
            key: ChordKey::Named(value),
            modifiers: ChordModifiers::SHIFT,
        }
    }

    pub(crate) fn with_modifiers(mut self, modifiers: Modifiers) -> Self {
        self.modifiers = ChordModifiers::from_modifiers(modifiers);
        self
    }

    fn from_key(context: KeyContext, key: &Key, modifiers: Modifiers) -> Self {
        let key = match key.as_ref() {
            Key::Character(value) => ChordKey::Character(value.to_lowercase()),
            Key::Named(value) => ChordKey::Named(value),
            Key::Unidentified => ChordKey::Other,
        };
        let modifiers = ChordModifiers::from_iced(context, &key, modifiers);
        Self { key, modifiers }
    }

    fn parse(value: &str) -> Result<Self, String> {
        let mut key = None;
        let mut modifiers = ChordModifiers::empty();
        for raw_part in value.split('+') {
            let part = raw_part.trim().to_lowercase();
            if part.is_empty() {
                return Err("empty key part".to_owned());
            }
            match part.as_str() {
                "shift" => set_modifier(&mut modifiers.shift, "shift")?,
                "ctrl" | "control" => set_modifier(&mut modifiers.control, "ctrl")?,
                "alt" => set_modifier(&mut modifiers.alt, "alt")?,
                "logo" | "super" | "meta" | "cmd" | "command" => {
                    set_modifier(&mut modifiers.logo, "logo")?;
                }
                _ if key.is_none() => key = Some(parse_key(&part)?),
                _ => return Err("multiple non-modifier keys".to_owned()),
            }
        }
        Ok(Self {
            key: key.ok_or_else(|| "missing key".to_owned())?,
            modifiers,
        })
    }

    fn label(&self) -> String {
        let mut parts = Vec::new();
        if self.modifiers.control {
            parts.push("ctrl".to_owned());
        }
        if self.modifiers.alt {
            parts.push("alt".to_owned());
        }
        if self.modifiers.shift {
            parts.push("shift".to_owned());
        }
        if self.modifiers.logo {
            parts.push("logo".to_owned());
        }
        parts.push(self.key.label());
        parts.join("+")
    }

    fn supported_in(self, context: KeyContext) -> bool {
        context == KeyContext::Global
            || self.modifiers == ChordModifiers::empty()
            || (matches!(self.key, ChordKey::Named(Named::Enter))
                && self.modifiers == ChordModifiers::SHIFT)
    }

    fn is_focus_navigation(&self) -> bool {
        matches!(self.key, ChordKey::Named(Named::Tab))
            && (self.modifiers == ChordModifiers::empty()
                || self.modifiers == ChordModifiers::SHIFT)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ChordKey {
    Character(String),
    Named(Named),
    Other,
}

impl ChordKey {
    fn label(&self) -> String {
        match self {
            Self::Character(value) => value.clone(),
            Self::Named(value) => named_key_label(*value).to_owned(),
            Self::Other => "unidentified".to_owned(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "keyboard modifiers mirror Iced's independent modifier flags"
)]
struct ChordModifiers {
    shift: bool,
    control: bool,
    alt: bool,
    logo: bool,
}

impl ChordModifiers {
    const SHIFT: Self = Self {
        shift: true,
        control: false,
        alt: false,
        logo: false,
    };

    const fn empty() -> Self {
        Self {
            shift: false,
            control: false,
            alt: false,
            logo: false,
        }
    }

    fn from_modifiers(modifiers: Modifiers) -> Self {
        Self {
            shift: modifiers.shift(),
            control: modifiers.control(),
            alt: modifiers.alt(),
            logo: modifiers.logo(),
        }
    }

    fn from_iced(context: KeyContext, key: &ChordKey, modifiers: Modifiers) -> Self {
        if context == KeyContext::Modal && !matches!(key, ChordKey::Named(Named::Enter)) {
            Self::empty()
        } else if context == KeyContext::Modal {
            Self {
                shift: modifiers.shift(),
                ..Self::empty()
            }
        } else {
            Self::from_modifiers(modifiers)
        }
    }
}

fn set_modifier(target: &mut bool, label: &'static str) -> Result<(), String> {
    if *target {
        Err(format!("duplicate `{label}` modifier"))
    } else {
        *target = true;
        Ok(())
    }
}

fn parse_key(value: &str) -> Result<ChordKey, String> {
    let named = match value {
        "esc" | "escape" => Some(Named::Escape),
        "enter" | "return" => Some(Named::Enter),
        "tab" => Some(Named::Tab),
        "space" => Some(Named::Space),
        "backspace" => Some(Named::Backspace),
        "delete" | "del" => Some(Named::Delete),
        "home" => Some(Named::Home),
        "end" => Some(Named::End),
        "pageup" | "page_up" => Some(Named::PageUp),
        "pagedown" | "page_down" => Some(Named::PageDown),
        "up" | "arrowup" | "arrow_up" => Some(Named::ArrowUp),
        "down" | "arrowdown" | "arrow_down" => Some(Named::ArrowDown),
        "left" | "arrowleft" | "arrow_left" => Some(Named::ArrowLeft),
        "right" | "arrowright" | "arrow_right" => Some(Named::ArrowRight),
        _ => None,
    };
    if let Some(named) = named {
        return Ok(ChordKey::Named(named));
    }
    let mut chars = value.chars();
    let first = chars.next().ok_or_else(|| "missing key".to_owned())?;
    if chars.next().is_some() {
        Err(format!("unknown key `{value}`"))
    } else {
        Ok(ChordKey::Character(first.to_string()))
    }
}

fn named_key_label(value: Named) -> &'static str {
    match value {
        Named::Escape => "escape",
        Named::Enter => "enter",
        Named::Tab => "tab",
        Named::Space => "space",
        Named::Backspace => "backspace",
        Named::Delete => "delete",
        Named::Home => "home",
        Named::End => "end",
        Named::PageUp => "pageup",
        Named::PageDown => "pagedown",
        Named::ArrowUp => "arrowup",
        Named::ArrowDown => "arrowdown",
        Named::ArrowLeft => "arrowleft",
        Named::ArrowRight => "arrowright",
        _ => "named",
    }
}

#[derive(Clone, Debug)]
pub(crate) struct KeyMap {
    bindings: Vec<KeyBinding>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct KeyBindingHelp {
    pub(crate) context: KeyContext,
    pub(crate) name: &'static str,
    pub(crate) chord: String,
}

impl KeyMap {
    pub(crate) fn action_for(&self, context: KeyContext, chord: &KeyChord) -> Option<KeyAction> {
        self.bindings
            .iter()
            .find(|binding| binding.context == context && binding.chord == *chord)
            .map(|binding| binding.action)
    }

    pub(crate) fn help_rows(&self) -> Vec<KeyBindingHelp> {
        self.bindings
            .iter()
            .map(|binding| KeyBindingHelp {
                context: binding.context,
                name: binding.id.config_name(),
                chord: binding.chord.label(),
            })
            .collect()
    }

    pub(crate) fn from_config(raw: &BTreeMap<String, String>) -> Result<Self, KeyMapError> {
        let mut keymap = Self::default();
        for (name, value) in raw {
            let id = BindingId::parse(name)
                .ok_or_else(|| KeyMapError::UnknownBinding { name: name.clone() })?;
            let chord = KeyChord::parse(value).map_err(|reason| KeyMapError::InvalidKey {
                binding: name.clone(),
                value: value.clone(),
                reason,
            })?;
            let binding = keymap
                .bindings
                .iter_mut()
                .find(|binding| binding.id == id)
                .expect("every binding id has one default");
            if chord.is_focus_navigation() {
                return Err(KeyMapError::InvalidKey {
                    binding: name.clone(),
                    value: value.clone(),
                    reason: "tab and shift+tab are reserved for focus navigation".to_owned(),
                });
            }
            if !chord.clone().supported_in(binding.context) {
                return Err(KeyMapError::InvalidKey {
                    binding: name.clone(),
                    value: value.clone(),
                    reason: "modal shortcuts only support bare keys, except shift+enter".to_owned(),
                });
            }
            binding.chord = chord;
        }
        keymap.validate_conflicts()?;
        Ok(keymap)
    }

    fn validate_conflicts(&self) -> Result<(), KeyMapError> {
        for (index, left) in self.bindings.iter().enumerate() {
            for right in self.bindings.iter().skip(index + 1) {
                if left.context == right.context
                    && left.chord == right.chord
                    && left.action != right.action
                {
                    return Err(KeyMapError::Conflict {
                        context: left.context,
                        chord: left.chord.label(),
                        first: left.id.config_name(),
                        second: right.id.config_name(),
                    });
                }
            }
        }
        Ok(())
    }
}

impl Default for KeyMap {
    fn default() -> Self {
        Self {
            bindings: vec![
                KeyBinding::global(
                    BindingId::OpenInbox,
                    KeyChord::character("i"),
                    KeyAction::OpenInbox,
                ),
                KeyBinding::global(
                    BindingId::OpenSelectedSession,
                    KeyChord::character("o"),
                    KeyAction::OpenSelectedSession,
                ),
                KeyBinding::global(
                    BindingId::ShowSelectedSession,
                    KeyChord::named(Named::Enter),
                    KeyAction::ShowSelectedSession,
                ),
                KeyBinding::global(
                    BindingId::OpenKeymapHelp,
                    KeyChord::character("?").with_modifiers(Modifiers::SHIFT),
                    KeyAction::OpenKeymapHelp,
                ),
                KeyBinding::global(
                    BindingId::NewSession,
                    KeyChord::character("n"),
                    KeyAction::NewSession,
                ),
                KeyBinding::global(
                    BindingId::OpenAssistant,
                    KeyChord::character("a"),
                    KeyAction::OpenAssistant,
                ),
                KeyBinding::modal(
                    BindingId::ModalBack,
                    KeyChord::named(Named::Escape),
                    KeyAction::ModalBack,
                ),
                KeyBinding::modal(
                    BindingId::ModalPrimary,
                    KeyChord::named(Named::Enter),
                    KeyAction::ModalPrimary,
                ),
                KeyBinding::modal(
                    BindingId::ModalPrimaryWithTerminal,
                    KeyChord::shift_named(Named::Enter),
                    KeyAction::ModalPrimaryWithTerminal,
                ),
                KeyBinding::modal(
                    BindingId::ModalListUp,
                    KeyChord::character("k"),
                    KeyAction::ListUp,
                ),
                KeyBinding::modal(
                    BindingId::ModalListUpArrow,
                    KeyChord::named(Named::ArrowUp),
                    KeyAction::ListUp,
                ),
                KeyBinding::modal(
                    BindingId::ModalListDown,
                    KeyChord::character("j"),
                    KeyAction::ListDown,
                ),
                KeyBinding::modal(
                    BindingId::ModalListDownArrow,
                    KeyChord::named(Named::ArrowDown),
                    KeyAction::ListDown,
                ),
                KeyBinding::modal(
                    BindingId::ModalOpenLinkedSession,
                    KeyChord::character("o"),
                    KeyAction::OpenLinkedSession,
                ),
            ],
        }
    }
}

#[derive(Clone, Debug)]
struct KeyBinding {
    id: BindingId,
    context: KeyContext,
    chord: KeyChord,
    action: KeyAction,
}

impl KeyBinding {
    fn global(id: BindingId, chord: KeyChord, action: KeyAction) -> Self {
        Self {
            id,
            context: KeyContext::Global,
            chord,
            action,
        }
    }

    fn modal(id: BindingId, chord: KeyChord, action: KeyAction) -> Self {
        Self {
            id,
            context: KeyContext::Modal,
            chord,
            action,
        }
    }
}

pub(crate) fn subscription() -> Subscription<Message> {
    keyboard::listen().filter_map(|event| match event {
        keyboard::Event::KeyPressed { key, modifiers, .. } => {
            Some(Message::KeyPressed { key, modifiers })
        }
        keyboard::Event::KeyReleased { .. } | keyboard::Event::ModifiersChanged(_) => None,
    })
}

pub(crate) fn route_key_press(app: &PohunekApp, key: &Key, modifiers: Modifiers) -> Vec<Message> {
    let context = if app.modal == ModalView::None {
        KeyContext::Global
    } else {
        KeyContext::Modal
    };
    let chord = KeyChord::from_key(context, key, modifiers);
    app.keymap
        .action_for(context, &chord)
        .map_or_else(Vec::new, |action| action_messages(app, action))
}

fn action_messages(app: &PohunekApp, action: KeyAction) -> Vec<Message> {
    match action {
        KeyAction::OpenInbox => vec![Message::OpenInbox],
        KeyAction::OpenSelectedSession => open_selected_session(app),
        KeyAction::ShowSelectedSession => show_selected_session(app),
        KeyAction::OpenKeymapHelp => vec![Message::OpenKeymapModal],
        KeyAction::NewSession => {
            if selected_project(app).is_some() {
                vec![Message::OpenStartModal]
            } else {
                Vec::new()
            }
        }
        KeyAction::OpenAssistant => vec![Message::OpenAssistantModal],
        KeyAction::ModalBack => vec![escape_message(app)],
        KeyAction::ModalPrimary => modal_primary(app, false),
        KeyAction::ModalPrimaryWithTerminal => modal_primary(app, true),
        KeyAction::OpenLinkedSession => selected_inbox_notification_link(app),
        KeyAction::ListUp => vec![Message::MoveListSelection(ListDirection::Up)],
        KeyAction::ListDown => vec![Message::MoveListSelection(ListDirection::Down)],
    }
}

fn open_selected_session(app: &PohunekApp) -> Vec<Message> {
    selected_session(app).map_or_else(Vec::new, |(host_id, session)| {
        let can_open = app.workspace.session_rows().into_iter().any(|row| {
            row.host_id == *host_id
                && row.session_id == session.id
                && matches!(
                    row.access,
                    pohunek_gui_core::SessionAccess::Attach
                        | pohunek_gui_core::SessionAccess::Resume
                )
        });
        if can_open {
            vec![Message::OpenSession {
                host_id: host_id.clone(),
                session_id: session.id.clone(),
            }]
        } else {
            Vec::new()
        }
    })
}

fn show_selected_session(app: &PohunekApp) -> Vec<Message> {
    selected_session(app).map_or_else(Vec::new, |(host_id, session)| {
        vec![Message::SelectSession {
            host_id: host_id.clone(),
            session_id: session.id.clone(),
        }]
    })
}

fn escape_message(app: &PohunekApp) -> Message {
    if app.modal == ModalView::Inbox && matches!(app.inbox_view, InboxView::Message { .. }) {
        Message::InboxBack
    } else {
        Message::CloseModal
    }
}

fn modal_primary(app: &PohunekApp, open_terminal: bool) -> Vec<Message> {
    match app.modal {
        ModalView::Start => vec![Message::CreateSession],
        ModalView::Assistant => vec![Message::LaunchAssistant],
        ModalView::Session => open_selected_session(app),
        ModalView::ConfirmDeleteSession => vec![Message::ConfirmDeleteSession],
        ModalView::Inbox => inbox_primary(app, open_terminal),
        ModalView::Keymap | ModalView::None => Vec::new(),
    }
}

fn inbox_primary(app: &PohunekApp, open_terminal: bool) -> Vec<Message> {
    let InboxView::Message {
        host_id,
        notification_id,
    } = &app.inbox_view
    else {
        return selected_inbox_notification(app);
    };
    let Some(record) = app.workspace.notification(host_id, notification_id) else {
        return Vec::new();
    };
    let Some(session_id) = record.session_id.clone() else {
        return Vec::new();
    };
    let exists = app
        .workspace
        .hosts
        .get(host_id)
        .is_some_and(|host| host.sessions.contains_key(&session_id.0));
    if !exists {
        return Vec::new();
    }
    let mut messages = vec![Message::OpenNotificationLink {
        host_id: host_id.clone(),
        notification_id: notification_id.clone(),
    }];
    if open_terminal {
        messages.push(Message::OpenSession {
            host_id: host_id.clone(),
            session_id,
        });
    }
    messages
}

fn selected_inbox_notification(app: &PohunekApp) -> Vec<Message> {
    selected_inbox_row(app).map_or_else(Vec::new, |row| {
        vec![Message::SelectNotification {
            host_id: row.host_id,
            notification_id: row.record.id,
        }]
    })
}

fn selected_inbox_notification_link(app: &PohunekApp) -> Vec<Message> {
    if app.modal != ModalView::Inbox || !matches!(app.inbox_view, InboxView::List) {
        return Vec::new();
    }
    selected_inbox_row(app).map_or_else(Vec::new, |row| {
        let exists = row.record.session_id.as_ref().is_some_and(|session_id| {
            app.workspace
                .hosts
                .get(&row.host_id)
                .is_some_and(|host| host.sessions.contains_key(&session_id.0))
        });
        if exists {
            vec![Message::OpenNotificationLink {
                host_id: row.host_id,
                notification_id: row.record.id,
            }]
        } else {
            Vec::new()
        }
    })
}

fn selected_inbox_row(app: &PohunekApp) -> Option<pohunek_gui_core::NotificationRow> {
    let rows = app
        .workspace
        .inbox_rows(app.inbox_scope, &app.notification_filter);
    app.inbox_cursor
        .as_ref()
        .and_then(|(host_id, id)| {
            rows.iter()
                .find(|row| &row.host_id == host_id && &row.record.id == id)
        })
        .or_else(|| rows.first())
        .cloned()
}

pub(crate) fn focus_task(app: &PohunekApp) -> Task<Message> {
    match app.modal {
        ModalView::Start => operation::focus(start_name_input_id()),
        ModalView::Assistant => operation::focus(assistant_request_input_id()),
        ModalView::None
        | ModalView::Session
        | ModalView::ConfirmDeleteSession
        | ModalView::Keymap
        | ModalView::Inbox => Task::none(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_keymap_contains_only_session_first_global_actions() {
        let keymap = KeyMap::default();
        assert_eq!(
            keymap.action_for(KeyContext::Global, &KeyChord::character("i")),
            Some(KeyAction::OpenInbox)
        );
        assert_eq!(
            keymap.action_for(KeyContext::Global, &KeyChord::character("n")),
            Some(KeyAction::NewSession)
        );
        assert_eq!(
            keymap.action_for(KeyContext::Global, &KeyChord::character("2")),
            None
        );
    }

    #[test]
    fn removed_tab_binding_names_are_rejected() {
        let raw = BTreeMap::from([("tab_linear".to_owned(), "2".to_owned())]);
        assert!(matches!(
            KeyMap::from_config(&raw),
            Err(KeyMapError::UnknownBinding { name }) if name == "tab_linear"
        ));
    }

    #[test]
    fn configured_conflicts_are_rejected_within_one_context() {
        let raw = BTreeMap::from([
            ("open_inbox".to_owned(), "x".to_owned()),
            ("new_session".to_owned(), "x".to_owned()),
        ]);
        assert!(matches!(
            KeyMap::from_config(&raw),
            Err(KeyMapError::Conflict {
                context: KeyContext::Global,
                ..
            })
        ));
    }

    #[test]
    fn modal_escape_steps_back_from_inbox_message() {
        let mut app = PohunekApp::test_default();
        app.modal = ModalView::Inbox;
        app.inbox_view = InboxView::Message {
            host_id: pohunek_gui_core::HostId::new("local"),
            notification_id: protocol::NotificationId("n-1".to_owned()),
        };
        assert!(matches!(escape_message(&app), Message::InboxBack));
    }
}
