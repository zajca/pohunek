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

use std::collections::BTreeMap;
use std::fmt;

use iced::keyboard::key::Named;
use iced::keyboard::{self, Key, Modifiers};
use iced::Subscription;
use pohunek_gui_core::{ProviderPanel, RightTab};

use crate::message::{InboxView, ListDirection, Message, ModalView};
use crate::selection::{
    selected_github_scope, selected_host_id, selected_session, tab_project_scope,
};
use crate::view::provider::{
    selected_github_issue_in_state, selected_linear_issue_in_state, selected_pull_request_in_state,
};
use crate::PohunekApp;

/// Keyboard routing scope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeyContext {
    Global,
    Modal,
}

/// Shortcut action resolved from a key chord.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeyAction {
    TabDetail,
    TabLinear,
    TabGitHub,
    TabWorktrees,
    OpenInbox,
    CycleBlocked,
    OpenSelectedSession,
    ActivateSelection,
    OpenKeymapHelp,
    NewSession,
    OpenAssistant,
    RefreshTab,
    ModalBack,
    ModalPrimary,
    ModalPrimaryWithTerminal,
    OpenLinkedSession,
    ListUp,
    ListDown,
    FocusSearch,
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
            Self::UnknownBinding { name } => {
                write!(f, "unknown keybinding `{name}`")
            }
            Self::InvalidKey {
                binding,
                value,
                reason,
            } => {
                write!(f, "invalid key `{value}` for `{binding}`: {reason}")
            }
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

/// Stable names accepted in the `[keybindings]` table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KeyBindingId {
    TabDetail,
    TabLinear,
    TabGitHub,
    TabWorktrees,
    OpenInbox,
    CycleBlocked,
    OpenSelectedSession,
    ActivateSelection,
    OpenKeymapHelp,
    ListUp,
    ListUpArrow,
    ListDown,
    ListDownArrow,
    FocusSearch,
    NewSession,
    OpenAssistant,
    RefreshTab,
    ModalBack,
    ModalPrimary,
    ModalPrimaryWithTerminal,
    ModalListUp,
    ModalListUpArrow,
    ModalListDown,
    ModalListDownArrow,
    ModalOpenLinkedSession,
}

impl KeyBindingId {
    const ALL: &'static [Self] = &[
        Self::TabDetail,
        Self::TabLinear,
        Self::TabGitHub,
        Self::TabWorktrees,
        Self::OpenInbox,
        Self::CycleBlocked,
        Self::OpenSelectedSession,
        Self::ActivateSelection,
        Self::OpenKeymapHelp,
        Self::ListUp,
        Self::ListUpArrow,
        Self::ListDown,
        Self::ListDownArrow,
        Self::FocusSearch,
        Self::NewSession,
        Self::OpenAssistant,
        Self::RefreshTab,
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
            Self::TabDetail => "tab_detail",
            Self::TabLinear => "tab_linear",
            Self::TabGitHub => "tab_github",
            Self::TabWorktrees => "tab_worktrees",
            Self::OpenInbox => "open_inbox",
            Self::CycleBlocked => "cycle_blocked",
            Self::OpenSelectedSession => "open_selected_session",
            Self::ActivateSelection => "activate_selection",
            Self::OpenKeymapHelp => "open_keymap_help",
            Self::ListUp => "list_up",
            Self::ListUpArrow => "list_up_arrow",
            Self::ListDown => "list_down",
            Self::ListDownArrow => "list_down_arrow",
            Self::FocusSearch => "focus_search",
            Self::NewSession => "new_session",
            Self::OpenAssistant => "open_assistant",
            Self::RefreshTab => "refresh_tab",
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

impl fmt::Display for KeyContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Global => f.write_str("global"),
            Self::Modal => f.write_str("modal"),
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
    /// Builds a bare character chord.
    pub(crate) fn character(value: &str) -> Self {
        Self {
            key: ChordKey::Character(normalize_character_key(value)),
            modifiers: ChordModifiers::empty(),
        }
    }

    /// Builds a bare named-key chord.
    pub(crate) fn named(value: Named) -> Self {
        Self {
            key: ChordKey::Named(value),
            modifiers: ChordModifiers::empty(),
        }
    }

    /// Builds a Shift-modified named-key chord.
    pub(crate) fn shift_named(value: Named) -> Self {
        Self {
            key: ChordKey::Named(value),
            modifiers: ChordModifiers::SHIFT,
        }
    }

    /// Returns this chord with all provided modifiers.
    pub(crate) fn with_modifiers(mut self, modifiers: Modifiers) -> Self {
        self.modifiers = ChordModifiers::from_modifiers(modifiers);
        self
    }

    fn from_key(context: KeyContext, key: &Key, modifiers: Modifiers) -> Self {
        let key = match key.as_ref() {
            Key::Character(value) => ChordKey::Character(normalize_character_key(value)),
            Key::Named(value) => ChordKey::Named(value),
            Key::Unidentified => ChordKey::Other,
        };
        let modifiers = ChordModifiers::from_iced(context, &key, modifiers);
        Self { key, modifiers }
    }

    fn parse_config(value: &str) -> Result<Self, String> {
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
                _ if key.is_none() => key = Some(parse_key_part(&part)?),
                _ => return Err("multiple non-modifier keys".to_owned()),
            }
        }
        let key = key.ok_or_else(|| "missing key".to_owned())?;
        Ok(Self { key, modifiers })
    }

    fn config_label(&self) -> String {
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
        parts.push(self.key.config_label());
        parts.join("+")
    }

    fn is_supported_in_context(&self, context: KeyContext) -> bool {
        match context {
            KeyContext::Global => true,
            KeyContext::Modal => {
                self.modifiers == ChordModifiers::empty()
                    || (matches!(self.key, ChordKey::Named(Named::Enter))
                        && self.modifiers == ChordModifiers::SHIFT)
            }
        }
    }
}

fn normalize_character_key(value: &str) -> String {
    value.to_lowercase()
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ChordKey {
    Character(String),
    Named(Named),
    Other,
}

impl ChordKey {
    fn config_label(&self) -> String {
        match self {
            Self::Character(value) => value.clone(),
            Self::Named(value) => named_key_label(*value).to_owned(),
            Self::Other => "unidentified".to_owned(),
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

fn parse_key_part(value: &str) -> Result<ChordKey, String> {
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
    let Some(first) = chars.next() else {
        return Err("missing key".to_owned());
    };
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "keyboard modifiers are independent bit flags, mirroring Iced's Modifiers API"
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
        match context {
            KeyContext::Global => Self::from_modifiers(modifiers),
            KeyContext::Modal if matches!(key, ChordKey::Named(Named::Enter)) => Self {
                shift: modifiers.shift(),
                control: false,
                alt: false,
                logo: false,
            },
            KeyContext::Modal => Self::empty(),
        }
    }
}

/// Keyboard shortcut map.
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
    /// Builds a keymap from explicit bindings.
    pub(crate) fn new(bindings: Vec<KeyBinding>) -> Self {
        Self { bindings }
    }

    /// Returns the action bound to a context and chord.
    pub(crate) fn action_for(&self, context: KeyContext, chord: &KeyChord) -> Option<KeyAction> {
        self.bindings
            .iter()
            .find(|binding| binding.context == context && binding.chord == *chord)
            .map(|binding| binding.action)
    }

    /// Rows for the in-app shortcut cheat sheet.
    pub(crate) fn help_rows(&self) -> Vec<KeyBindingHelp> {
        self.bindings
            .iter()
            .filter_map(|binding| {
                Some(KeyBindingHelp {
                    context: binding.context,
                    name: binding.id?.config_name(),
                    chord: binding.chord.config_label(),
                })
            })
            .collect()
    }

    /// Builds the default keymap with `[keybindings]` overrides applied.
    pub(crate) fn from_config(raw: &BTreeMap<String, String>) -> Result<Self, KeyMapError> {
        let mut keymap = Self::default();
        for (name, value) in raw {
            let id = KeyBindingId::parse(name)
                .ok_or_else(|| KeyMapError::UnknownBinding { name: name.clone() })?;
            let chord =
                KeyChord::parse_config(value).map_err(|reason| KeyMapError::InvalidKey {
                    binding: name.clone(),
                    value: value.clone(),
                    reason,
                })?;
            let Some(index) = keymap
                .bindings
                .iter()
                .position(|binding| binding.id == Some(id))
            else {
                unreachable!("every KeyBindingId must have one default binding");
            };
            let context = keymap.bindings[index].context;
            if !chord.is_supported_in_context(context) {
                return Err(KeyMapError::InvalidKey {
                    binding: name.clone(),
                    value: value.clone(),
                    reason: "modal shortcuts only support bare keys, except shift+enter".to_owned(),
                });
            }
            keymap.bindings[index].chord = chord;
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
                        chord: left.chord.config_label(),
                        first: left.config_name(),
                        second: right.config_name(),
                    });
                }
            }
        }
        Ok(())
    }
}

impl Default for KeyMap {
    fn default() -> Self {
        Self::new(vec![
            default_global_character(KeyBindingId::TabDetail, "1", KeyAction::TabDetail),
            default_global_character(KeyBindingId::TabLinear, "2", KeyAction::TabLinear),
            default_global_character(KeyBindingId::TabGitHub, "3", KeyAction::TabGitHub),
            default_global_character(KeyBindingId::TabWorktrees, "4", KeyAction::TabWorktrees),
            default_global_character(KeyBindingId::OpenInbox, "i", KeyAction::OpenInbox),
            default_global_character(KeyBindingId::CycleBlocked, "b", KeyAction::CycleBlocked),
            default_global_character(
                KeyBindingId::OpenSelectedSession,
                "o",
                KeyAction::OpenSelectedSession,
            ),
            default_global_named(
                KeyBindingId::ActivateSelection,
                Named::Enter,
                KeyAction::ActivateSelection,
            ),
            default_global_chord(
                KeyBindingId::OpenKeymapHelp,
                KeyChord::character("?").with_modifiers(Modifiers::SHIFT),
                KeyAction::OpenKeymapHelp,
            ),
            default_global_named(KeyBindingId::ListUpArrow, Named::ArrowUp, KeyAction::ListUp),
            default_global_named(
                KeyBindingId::ListDownArrow,
                Named::ArrowDown,
                KeyAction::ListDown,
            ),
            default_global_character(KeyBindingId::ListUp, "k", KeyAction::ListUp),
            default_global_character(KeyBindingId::ListDown, "j", KeyAction::ListDown),
            default_global_character(KeyBindingId::FocusSearch, "/", KeyAction::FocusSearch),
            default_global_character(KeyBindingId::NewSession, "n", KeyAction::NewSession),
            default_global_character(KeyBindingId::OpenAssistant, "a", KeyAction::OpenAssistant),
            default_global_character(KeyBindingId::RefreshTab, "r", KeyAction::RefreshTab),
            default_modal_named(KeyBindingId::ModalBack, Named::Escape, KeyAction::ModalBack),
            default_modal_named(
                KeyBindingId::ModalPrimary,
                Named::Enter,
                KeyAction::ModalPrimary,
            ),
            default_modal_shift_named(
                KeyBindingId::ModalPrimaryWithTerminal,
                Named::Enter,
                KeyAction::ModalPrimaryWithTerminal,
            ),
            default_modal_named(
                KeyBindingId::ModalListUpArrow,
                Named::ArrowUp,
                KeyAction::ListUp,
            ),
            default_modal_named(
                KeyBindingId::ModalListDownArrow,
                Named::ArrowDown,
                KeyAction::ListDown,
            ),
            default_modal_character(KeyBindingId::ModalListUp, "k", KeyAction::ListUp),
            default_modal_character(KeyBindingId::ModalListDown, "j", KeyAction::ListDown),
            default_modal_character(
                KeyBindingId::ModalOpenLinkedSession,
                "o",
                KeyAction::OpenLinkedSession,
            ),
        ])
    }
}

#[derive(Clone, Debug)]
pub(crate) struct KeyBinding {
    id: Option<KeyBindingId>,
    context: KeyContext,
    chord: KeyChord,
    action: KeyAction,
}

impl KeyBinding {
    /// Builds one key binding.
    #[cfg(test)]
    pub(crate) fn new(context: KeyContext, chord: KeyChord, action: KeyAction) -> Self {
        Self {
            id: None,
            context,
            chord,
            action,
        }
    }

    fn default(id: KeyBindingId, context: KeyContext, chord: KeyChord, action: KeyAction) -> Self {
        Self {
            id: Some(id),
            context,
            chord,
            action,
        }
    }

    fn config_name(&self) -> &'static str {
        self.id.map_or("custom", KeyBindingId::config_name)
    }
}

fn default_global_character(id: KeyBindingId, value: &str, action: KeyAction) -> KeyBinding {
    KeyBinding::default(id, KeyContext::Global, KeyChord::character(value), action)
}

fn default_global_named(id: KeyBindingId, value: Named, action: KeyAction) -> KeyBinding {
    KeyBinding::default(id, KeyContext::Global, KeyChord::named(value), action)
}

fn default_global_chord(id: KeyBindingId, chord: KeyChord, action: KeyAction) -> KeyBinding {
    KeyBinding::default(id, KeyContext::Global, chord, action)
}

fn default_modal_character(id: KeyBindingId, value: &str, action: KeyAction) -> KeyBinding {
    KeyBinding::default(id, KeyContext::Modal, KeyChord::character(value), action)
}

fn default_modal_named(id: KeyBindingId, value: Named, action: KeyAction) -> KeyBinding {
    KeyBinding::default(id, KeyContext::Modal, KeyChord::named(value), action)
}

fn default_modal_shift_named(id: KeyBindingId, value: Named, action: KeyAction) -> KeyBinding {
    KeyBinding::default(id, KeyContext::Modal, KeyChord::shift_named(value), action)
}

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
    route_key_press_with_keymap(app, &app.keymap, key, modifiers)
}

/// Routes one key press through the provided keymap.
pub(crate) fn route_key_press_with_keymap(
    app: &PohunekApp,
    keymap: &KeyMap,
    key: &Key,
    modifiers: Modifiers,
) -> Vec<Message> {
    if app.modal == ModalView::None {
        route_key_in_context(app, keymap, KeyContext::Global, key, modifiers)
    } else {
        route_key_in_context(app, keymap, KeyContext::Modal, key, modifiers)
    }
}

fn route_key_in_context(
    app: &PohunekApp,
    keymap: &KeyMap,
    context: KeyContext,
    key: &Key,
    modifiers: Modifiers,
) -> Vec<Message> {
    let chord = KeyChord::from_key(context, key, modifiers);
    keymap
        .action_for(context, &chord)
        .map_or_else(Vec::new, |action| action_to_messages(app, action))
}

fn action_to_messages(app: &PohunekApp, action: KeyAction) -> Vec<Message> {
    match action {
        KeyAction::TabDetail => vec![Message::SelectTab(RightTab::Detail)],
        KeyAction::TabLinear => tab_shortcut(app, RightTab::Linear),
        KeyAction::TabGitHub => tab_shortcut(app, RightTab::GitHub),
        KeyAction::TabWorktrees => tab_shortcut(app, RightTab::Worktrees),
        KeyAction::OpenInbox => vec![Message::OpenInbox],
        KeyAction::CycleBlocked => vec![Message::CycleBlockedAgent],
        KeyAction::OpenSelectedSession => open_selected_session_or_active_item(app),
        KeyAction::ActivateSelection => activate_selected_item(app),
        KeyAction::OpenKeymapHelp => vec![Message::OpenKeymapModal],
        KeyAction::NewSession => vec![Message::OpenStartModal],
        KeyAction::OpenAssistant => vec![Message::OpenAssistantModal],
        KeyAction::RefreshTab => refresh_active_tab(app),
        KeyAction::ModalBack => vec![escape_message(app)],
        KeyAction::ModalPrimary => enter_messages(app, false),
        KeyAction::ModalPrimaryWithTerminal => enter_messages(app, true),
        KeyAction::OpenLinkedSession => selected_inbox_notification_link(app),
        KeyAction::ListUp => vec![Message::MoveListSelection(ListDirection::Up)],
        KeyAction::ListDown => vec![Message::MoveListSelection(ListDirection::Down)],
        KeyAction::FocusSearch => vec![Message::FocusProviderSearch],
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

/// `o`: activate the provider list item under the right-pane cursor when a
/// provider tab is active; otherwise keep the B4 behavior of opening the
/// selected session in a terminal.
fn open_selected_session_or_active_item(app: &PohunekApp) -> Vec<Message> {
    activate_selected_provider_item(app).unwrap_or_else(|| open_selected_session(app))
}

/// `Enter`: activate the selected row in the current list.
fn activate_selected_item(app: &PohunekApp) -> Vec<Message> {
    if app.modal == ModalView::Inbox && matches!(app.inbox_view, InboxView::List) {
        return selected_inbox_notification(app);
    }
    if app.modal == ModalView::None {
        return activate_selected_provider_item(app).unwrap_or_else(|| open_selected_session(app));
    }
    Vec::new()
}

fn activate_selected_provider_item(app: &PohunekApp) -> Option<Vec<Message>> {
    let Ok(host_id) = selected_host_id(app) else {
        return None;
    };
    let host = app.workspace.hosts.get(&host_id)?;
    match app.ui_state.active_tab {
        RightTab::Linear => selected_linear_issue_in_state(&host.provider.linear)
            .map(|issue| vec![Message::OpenLinearIssue(issue.prompt_item_id().to_owned())]),
        RightTab::GitHub if github_provider_scope_matches(app, host) => {
            selected_pull_request_in_state(&host.provider.github)
                .map(|pull_request| vec![Message::OpenGitHubPullRequest(pull_request.number)])
                .or_else(|| {
                    selected_github_issue_in_state(&host.provider.github)
                        .map(|issue| vec![Message::OpenGitHubIssue(issue.number)])
                })
        }
        RightTab::Detail | RightTab::GitHub | RightTab::Worktrees => None,
    }
}

fn github_provider_scope_matches(app: &PohunekApp, host: &pohunek_gui_core::HostView) -> bool {
    selected_github_scope(app)
        .is_ok_and(|scope| host.provider.github.scope.as_ref() == Some(&scope))
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
        let has_live_session = row.record.session_id.as_ref().is_some_and(|session_id| {
            app.workspace
                .hosts
                .get(&row.host_id)
                .is_some_and(|host| host.sessions.contains_key(&session_id.0))
        });
        if has_live_session {
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
    let selected = app
        .inbox_cursor
        .as_ref()
        .and_then(|(host_id, id)| {
            rows.iter()
                .find(|row| &row.host_id == host_id && &row.record.id == id)
        })
        .or_else(|| rows.first());
    selected.cloned()
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
fn enter_messages(app: &PohunekApp, open_terminal: bool) -> Vec<Message> {
    match app.modal {
        ModalView::Start => vec![Message::CreateSession],
        ModalView::Assistant => vec![Message::LaunchAssistant],
        ModalView::ProviderItem => provider_item_enter(app),
        ModalView::Inbox => inbox_enter(app, open_terminal),
        // `route_key_press` only calls into modal-scoped routing when a modal
        // is open; kept explicit (no wildcard) so a future `ModalView`
        // variant fails to compile here instead of silently doing nothing.
        ModalView::Keymap | ModalView::None => Vec::new(),
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
fn inbox_enter(app: &PohunekApp, open_terminal: bool) -> Vec<Message> {
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
    if open_terminal {
        messages.push(Message::OpenSession {
            host_id: host_id.clone(),
            session_id,
        });
    }
    messages
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use pohunek_gui_core::{
        providers, ConnState, GitHubProviderScope, HostId, NotificationFilter, NotificationScope,
        Selection,
    };
    use protocol::{
        AgentKind, NotificationId, NotificationKind, NotificationRecord, NotificationSeverity,
        NotificationSource, NotificationStatus, ProjectInfo, ProjectSource, SessionId, SessionInfo,
        SessionState, StateSource,
    };

    use super::*;
    use crate::message::{AssistantForm, MetadataEdit, ProjectEdit, StartForm, TemplateRecipe};

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the default keymap audit keeps every documented binding in one assertion block"
    )]
    fn default_keymap_contains_current_b4_shortcuts() {
        let keymap = KeyMap::default();

        assert_eq!(
            keymap.action_for(KeyContext::Global, &KeyChord::character("1")),
            Some(KeyAction::TabDetail)
        );
        assert_eq!(
            keymap.action_for(KeyContext::Global, &KeyChord::character("2")),
            Some(KeyAction::TabLinear)
        );
        assert_eq!(
            keymap.action_for(KeyContext::Global, &KeyChord::character("3")),
            Some(KeyAction::TabGitHub)
        );
        assert_eq!(
            keymap.action_for(KeyContext::Global, &KeyChord::character("4")),
            Some(KeyAction::TabWorktrees)
        );
        assert_eq!(
            keymap.action_for(KeyContext::Global, &KeyChord::character("i")),
            Some(KeyAction::OpenInbox)
        );
        assert_eq!(
            keymap.action_for(KeyContext::Global, &KeyChord::character("b")),
            Some(KeyAction::CycleBlocked)
        );
        assert_eq!(
            keymap.action_for(KeyContext::Global, &KeyChord::character("o")),
            Some(KeyAction::OpenSelectedSession)
        );
        assert_eq!(
            keymap.action_for(KeyContext::Global, &KeyChord::named(Named::Enter)),
            Some(KeyAction::ActivateSelection)
        );
        assert_eq!(
            keymap.action_for(
                KeyContext::Global,
                &KeyChord::character("?").with_modifiers(Modifiers::SHIFT)
            ),
            Some(KeyAction::OpenKeymapHelp)
        );
        assert_eq!(
            keymap.action_for(KeyContext::Global, &KeyChord::named(Named::ArrowUp)),
            Some(KeyAction::ListUp)
        );
        assert_eq!(
            keymap.action_for(KeyContext::Global, &KeyChord::named(Named::ArrowDown)),
            Some(KeyAction::ListDown)
        );
        assert_eq!(
            keymap.action_for(KeyContext::Global, &KeyChord::character("k")),
            Some(KeyAction::ListUp)
        );
        assert_eq!(
            keymap.action_for(KeyContext::Global, &KeyChord::character("j")),
            Some(KeyAction::ListDown)
        );
        assert_eq!(
            keymap.action_for(KeyContext::Global, &KeyChord::character("/")),
            Some(KeyAction::FocusSearch)
        );
        assert_eq!(
            keymap.action_for(KeyContext::Global, &KeyChord::character("n")),
            Some(KeyAction::NewSession)
        );
        assert_eq!(
            keymap.action_for(KeyContext::Global, &KeyChord::character("a")),
            Some(KeyAction::OpenAssistant)
        );
        assert_eq!(
            keymap.action_for(KeyContext::Global, &KeyChord::character("r")),
            Some(KeyAction::RefreshTab)
        );
        assert_eq!(
            keymap.action_for(KeyContext::Modal, &KeyChord::named(Named::Escape)),
            Some(KeyAction::ModalBack)
        );
        assert_eq!(
            keymap.action_for(KeyContext::Modal, &KeyChord::named(Named::Enter)),
            Some(KeyAction::ModalPrimary)
        );
        assert_eq!(
            keymap.action_for(KeyContext::Modal, &KeyChord::shift_named(Named::Enter)),
            Some(KeyAction::ModalPrimaryWithTerminal)
        );
        assert_eq!(
            keymap.action_for(KeyContext::Modal, &KeyChord::named(Named::ArrowUp)),
            Some(KeyAction::ListUp)
        );
        assert_eq!(
            keymap.action_for(KeyContext::Modal, &KeyChord::named(Named::ArrowDown)),
            Some(KeyAction::ListDown)
        );
        assert_eq!(
            keymap.action_for(KeyContext::Modal, &KeyChord::character("k")),
            Some(KeyAction::ListUp)
        );
        assert_eq!(
            keymap.action_for(KeyContext::Modal, &KeyChord::character("j")),
            Some(KeyAction::ListDown)
        );
        assert_eq!(
            keymap.action_for(KeyContext::Modal, &KeyChord::character("o")),
            Some(KeyAction::OpenLinkedSession)
        );
    }

    #[test]
    fn default_keymap_routes_global_tab_shortcuts_like_b4() {
        let app = app_with_project_selection();
        let keymap = KeyMap::default();

        assert_select_tab(
            route_key_press_with_keymap(
                &app,
                &keymap,
                &Key::Character("1".into()),
                Modifiers::empty(),
            )
            .as_slice(),
            RightTab::Detail,
        );
        assert_select_tab(
            route_key_press_with_keymap(
                &app,
                &keymap,
                &Key::Character("2".into()),
                Modifiers::empty(),
            )
            .as_slice(),
            RightTab::Linear,
        );
        assert_select_tab(
            route_key_press_with_keymap(
                &app,
                &keymap,
                &Key::Character("3".into()),
                Modifiers::empty(),
            )
            .as_slice(),
            RightTab::GitHub,
        );
    }

    #[test]
    fn default_keymap_preserves_global_modifier_noop_guard() {
        let app = app_with_project_selection();
        let keymap = KeyMap::default();

        let messages = route_key_press_with_keymap(
            &app,
            &keymap,
            &Key::Character("1".into()),
            Modifiers::SHIFT,
        );

        assert!(messages.is_empty());
    }

    #[test]
    fn custom_keymap_can_route_modified_global_chords() {
        let mut app = app_with_project_selection();
        app.ui_state.active_tab = RightTab::GitHub;
        let keymap = KeyMap::new(vec![KeyBinding::new(
            KeyContext::Global,
            KeyChord::character("r").with_modifiers(Modifiers::CTRL),
            KeyAction::RefreshTab,
        )]);

        let messages = route_key_press_with_keymap(
            &app,
            &keymap,
            &Key::Character("r".into()),
            Modifiers::CTRL,
        );

        assert_eq!(messages.len(), 2);
        assert!(matches!(&messages[0], Message::FetchGitHubPullRequests));
        assert!(matches!(&messages[1], Message::FetchGitHubIssues));
    }

    #[test]
    fn keybindings_shift_character_override_matches_runtime_uppercase_character() {
        let mut app = app_with_project_selection();
        app.ui_state.active_tab = RightTab::GitHub;
        let raw = BTreeMap::from([("refresh_tab".to_owned(), "shift+r".to_owned())]);
        let keymap = KeyMap::from_config(&raw).expect("keymap");

        let messages = route_key_press_with_keymap(
            &app,
            &keymap,
            &Key::Character("R".into()),
            Modifiers::SHIFT,
        );

        assert_eq!(messages.len(), 2);
        assert!(matches!(&messages[0], Message::FetchGitHubPullRequests));
        assert!(matches!(&messages[1], Message::FetchGitHubIssues));
    }

    #[test]
    fn keybindings_partial_override_keeps_unlisted_defaults() {
        let raw = BTreeMap::from([("open_inbox".to_owned(), "ctrl+i".to_owned())]);

        let keymap = KeyMap::from_config(&raw).expect("keymap");

        assert_eq!(
            keymap.action_for(
                KeyContext::Global,
                &KeyChord::character("i").with_modifiers(Modifiers::CTRL)
            ),
            Some(KeyAction::OpenInbox)
        );
        assert_eq!(
            keymap.action_for(KeyContext::Global, &KeyChord::character("i")),
            None
        );
        assert_eq!(
            keymap.action_for(KeyContext::Global, &KeyChord::character("b")),
            Some(KeyAction::CycleBlocked)
        );
        assert!(keymap
            .help_rows()
            .iter()
            .any(|row| row.name == "open_inbox" && row.chord == "ctrl+i"));
    }

    #[test]
    fn keybindings_reject_unknown_binding_name() {
        let raw = BTreeMap::from([("unknown_action".to_owned(), "u".to_owned())]);

        let err = KeyMap::from_config(&raw).expect_err("unknown binding");

        assert!(matches!(
            err,
            KeyMapError::UnknownBinding { name } if name == "unknown_action"
        ));
    }

    #[test]
    fn keybindings_reject_bad_key_string() {
        let raw = BTreeMap::from([("open_inbox".to_owned(), "ctrl+nope".to_owned())]);

        let err = KeyMap::from_config(&raw).expect_err("bad key");

        assert!(matches!(
            err,
            KeyMapError::InvalidKey { binding, .. } if binding == "open_inbox"
        ));
    }

    #[test]
    fn keybindings_reject_same_context_conflict() {
        let raw = BTreeMap::from([("open_inbox".to_owned(), "b".to_owned())]);

        let err = KeyMap::from_config(&raw).expect_err("conflict");

        assert!(matches!(
            err,
            KeyMapError::Conflict {
                context: KeyContext::Global,
                first: "open_inbox",
                second: "cycle_blocked",
                ..
            }
        ));
    }

    #[test]
    fn keybindings_reject_unsupported_modal_modifier() {
        let raw = BTreeMap::from([("modal_back".to_owned(), "ctrl+escape".to_owned())]);

        let err = KeyMap::from_config(&raw).expect_err("unsupported modal modifier");

        assert!(matches!(
            err,
            KeyMapError::InvalidKey { binding, .. } if binding == "modal_back"
        ));
    }

    #[test]
    fn keybindings_allow_modal_and_global_chord_reuse() {
        let raw = BTreeMap::from([("open_inbox".to_owned(), "escape".to_owned())]);

        let keymap = KeyMap::from_config(&raw).expect("keymap");

        assert_eq!(
            keymap.action_for(KeyContext::Global, &KeyChord::named(Named::Escape)),
            Some(KeyAction::OpenInbox)
        );
        assert_eq!(
            keymap.action_for(KeyContext::Modal, &KeyChord::named(Named::Escape)),
            Some(KeyAction::ModalBack)
        );
    }

    #[test]
    fn default_keymap_routes_b5_list_navigation_shortcuts() {
        let app = app_with_project_selection();
        let keymap = KeyMap::default();

        assert!(matches!(
            route_key_press_with_keymap(
                &app,
                &keymap,
                &Key::Character("k".into()),
                Modifiers::empty()
            )
            .as_slice(),
            [Message::MoveListSelection(ListDirection::Up)]
        ));
        assert!(matches!(
            route_key_press_with_keymap(
                &app,
                &keymap,
                &Key::Character("j".into()),
                Modifiers::empty()
            )
            .as_slice(),
            [Message::MoveListSelection(ListDirection::Down)]
        ));
        assert!(matches!(
            route_key_press_with_keymap(
                &app,
                &keymap,
                &Key::Named(Named::ArrowUp),
                Modifiers::empty()
            )
            .as_slice(),
            [Message::MoveListSelection(ListDirection::Up)]
        ));
        assert!(matches!(
            route_key_press_with_keymap(
                &app,
                &keymap,
                &Key::Named(Named::ArrowDown),
                Modifiers::empty()
            )
            .as_slice(),
            [Message::MoveListSelection(ListDirection::Down)]
        ));
        assert!(matches!(
            route_key_press_with_keymap(
                &app,
                &keymap,
                &Key::Character("/".into()),
                Modifiers::empty()
            )
            .as_slice(),
            [Message::FocusProviderSearch]
        ));
    }

    #[test]
    fn default_keymap_routes_keymap_help_shortcut() {
        let app = app_without_selection();
        let keymap = KeyMap::default();

        let messages = route_key_press_with_keymap(
            &app,
            &keymap,
            &Key::Character("?".into()),
            Modifiers::SHIFT,
        );

        assert!(matches!(messages.as_slice(), [Message::OpenKeymapModal]));
    }

    #[test]
    fn default_keymap_activates_selected_linear_issue() {
        let host_id = HostId::new("local");
        let mut app = app_with_project_selection();
        app.ui_state.active_tab = RightTab::Linear;
        let host = app.workspace.hosts.get_mut(&host_id).expect("host");
        host.provider.linear.issues = vec![test_linear_issue("ENG-7", "Keyboard navigation")];
        host.provider.linear.selected_issue_id = Some("ENG-7".to_owned());
        let keymap = KeyMap::default();

        let messages = route_key_press_with_keymap(
            &app,
            &keymap,
            &Key::Named(Named::Enter),
            Modifiers::empty(),
        );

        assert!(matches!(
            messages.as_slice(),
            [Message::OpenLinearIssue(issue_id)] if issue_id == "ENG-7"
        ));
    }

    #[test]
    fn default_keymap_activates_selected_inbox_row() {
        let host_id = HostId::new("local");
        let session_id = SessionId("s-1".to_owned());
        let notification_id = NotificationId("n-1".to_owned());
        let mut app = app_with_live_notification(&host_id, &session_id, &notification_id);
        app.modal = ModalView::Inbox;
        app.inbox_view = InboxView::List;
        app.inbox_cursor = Some((host_id.clone(), notification_id.clone()));
        let keymap = KeyMap::default();

        let messages = route_key_press_with_keymap(
            &app,
            &keymap,
            &Key::Named(Named::Enter),
            Modifiers::empty(),
        );

        assert!(matches!(
            messages.as_slice(),
            [Message::SelectNotification {
                host_id: actual_host,
                notification_id: actual_notification,
            }] if actual_host == &host_id && actual_notification == &notification_id
        ));
    }

    #[test]
    fn default_keymap_o_opens_selected_inbox_row_linked_session() {
        let host_id = HostId::new("local");
        let session_id = SessionId("s-1".to_owned());
        let notification_id = NotificationId("n-1".to_owned());
        let mut app = app_with_live_notification(&host_id, &session_id, &notification_id);
        app.modal = ModalView::Inbox;
        app.inbox_view = InboxView::List;
        app.inbox_cursor = Some((host_id.clone(), notification_id.clone()));
        let keymap = KeyMap::default();

        let messages = route_key_press_with_keymap(
            &app,
            &keymap,
            &Key::Character("o".into()),
            Modifiers::empty(),
        );

        assert!(matches!(
            messages.as_slice(),
            [Message::OpenNotificationLink {
                host_id: actual_host,
                notification_id: actual_notification,
            }] if actual_host == &host_id && actual_notification == &notification_id
        ));
    }

    #[test]
    fn default_keymap_o_ignores_inbox_rows_outside_inbox_list() {
        let host_id = HostId::new("local");
        let session_id = SessionId("s-1".to_owned());
        let notification_id = NotificationId("n-1".to_owned());
        let mut app = app_with_live_notification(&host_id, &session_id, &notification_id);
        app.modal = ModalView::Start;
        let keymap = KeyMap::default();

        let messages = route_key_press_with_keymap(
            &app,
            &keymap,
            &Key::Character("o".into()),
            Modifiers::empty(),
        );

        assert!(messages.is_empty());
    }

    #[test]
    fn default_keymap_does_not_activate_filtered_out_linear_issue() {
        let host_id = HostId::new("local");
        let mut app = app_with_project_selection();
        app.ui_state.active_tab = RightTab::Linear;
        let host = app.workspace.hosts.get_mut(&host_id).expect("host");
        host.provider.linear.search = "visible".to_owned();
        host.provider.linear.issues = vec![
            test_linear_issue("ENG-7", "Hidden keyboard navigation"),
            test_linear_issue("ENG-8", "Visible provider row"),
        ];
        host.provider.linear.selected_issue_id = Some("ENG-7".to_owned());
        let keymap = KeyMap::default();

        let messages = route_key_press_with_keymap(
            &app,
            &keymap,
            &Key::Named(Named::Enter),
            Modifiers::empty(),
        );

        assert!(messages.is_empty());
    }

    #[test]
    fn default_keymap_activates_visible_github_issue_over_filtered_pull_request() {
        let host_id = HostId::new("local");
        let mut app = app_with_project_selection();
        app.ui_state.active_tab = RightTab::GitHub;
        let host = app.workspace.hosts.get_mut(&host_id).expect("host");
        host.provider.github.scope = Some(test_github_scope());
        host.provider.github.search = "visible issue".to_owned();
        host.provider.github.pull_requests = vec![test_github_pull_request(
            7,
            "Hidden pull request",
            "feature/hidden-pr",
        )];
        host.provider.github.issues = vec![test_github_issue(11, "Visible issue")];
        host.provider.github.selected_pull_request = Some(7);
        host.provider.github.selected_issue = Some(11);
        let keymap = KeyMap::default();

        let messages = route_key_press_with_keymap(
            &app,
            &keymap,
            &Key::Named(Named::Enter),
            Modifiers::empty(),
        );

        assert!(matches!(
            messages.as_slice(),
            [Message::OpenGitHubIssue(number)] if *number == 11
        ));
    }

    #[test]
    fn default_keymap_does_not_activate_github_item_from_stale_scope() {
        let host_id = HostId::new("local");
        let mut app = app_with_project_selection();
        app.ui_state.active_tab = RightTab::GitHub;
        let host = app.workspace.hosts.get_mut(&host_id).expect("host");
        host.provider.github.scope = Some(GitHubProviderScope::new("old-project", "/tmp/old"));
        host.provider.github.pull_requests = vec![test_github_pull_request(
            7,
            "Hidden pull request",
            "feature/hidden-pr",
        )];
        host.provider.github.selected_pull_request = Some(7);
        let keymap = KeyMap::default();

        let messages = route_key_press_with_keymap(
            &app,
            &keymap,
            &Key::Named(Named::Enter),
            Modifiers::empty(),
        );

        assert!(messages.is_empty());
    }

    #[test]
    fn default_keymap_o_still_opens_selected_session_without_provider_item() {
        let host_id = HostId::new("local");
        let session_id = SessionId("s-1".to_owned());
        let notification_id = NotificationId("n-1".to_owned());
        let mut app = app_with_live_notification(&host_id, &session_id, &notification_id);
        app.ui_state.selection = Some(Selection::Session {
            host_id: host_id.clone(),
            session_id: session_id.clone(),
        });
        app.workspace.selection.clone_from(&app.ui_state.selection);
        let keymap = KeyMap::default();

        let messages = route_key_press_with_keymap(
            &app,
            &keymap,
            &Key::Character("o".into()),
            Modifiers::empty(),
        );

        assert!(matches!(
            messages.as_slice(),
            [Message::OpenSession {
                host_id: actual_host,
                session_id: actual_session,
            }] if actual_host == &host_id && actual_session == &session_id
        ));
    }

    #[test]
    fn default_keymap_routes_github_refresh_in_b4_order() {
        let mut app = app_with_project_selection();
        app.ui_state.active_tab = RightTab::GitHub;
        let keymap = KeyMap::default();

        let messages = route_key_press_with_keymap(
            &app,
            &keymap,
            &Key::Character("r".into()),
            Modifiers::empty(),
        );

        assert_eq!(messages.len(), 2);
        assert!(matches!(&messages[0], Message::FetchGitHubPullRequests));
        assert!(matches!(&messages[1], Message::FetchGitHubIssues));
    }

    #[test]
    fn default_keymap_routes_modal_escape_like_b4() {
        let mut app = app_without_selection();
        app.modal = ModalView::Inbox;
        app.inbox_view = InboxView::Message {
            host_id: HostId::new("local"),
            notification_id: NotificationId("n-1".to_owned()),
        };
        let keymap = KeyMap::default();

        let messages =
            route_key_press_with_keymap(&app, &keymap, &Key::Named(Named::Escape), Modifiers::CTRL);

        assert_eq!(messages.len(), 1);
        assert!(matches!(&messages[0], Message::InboxBack));

        let messages = route_key_press_with_keymap(
            &app,
            &keymap,
            &Key::Named(Named::Escape),
            Modifiers::SHIFT,
        );

        assert_eq!(messages.len(), 1);
        assert!(matches!(&messages[0], Message::InboxBack));
    }

    #[test]
    fn default_keymap_routes_modal_enter_like_b4() {
        let mut app = app_without_selection();
        app.modal = ModalView::Start;
        let keymap = KeyMap::default();

        let messages =
            route_key_press_with_keymap(&app, &keymap, &Key::Named(Named::Enter), Modifiers::CTRL);

        assert_eq!(messages.len(), 1);
        assert!(matches!(&messages[0], Message::CreateSession));
    }

    #[test]
    fn default_keymap_routes_inbox_shift_enter_in_b4_order() {
        let host_id = HostId::new("local");
        let session_id = SessionId("s-1".to_owned());
        let notification_id = NotificationId("n-1".to_owned());
        let mut app = app_with_live_notification(&host_id, &session_id, &notification_id);
        app.modal = ModalView::Inbox;
        app.inbox_view = InboxView::Message {
            host_id: host_id.clone(),
            notification_id: notification_id.clone(),
        };
        let keymap = KeyMap::default();

        let messages =
            route_key_press_with_keymap(&app, &keymap, &Key::Named(Named::Enter), Modifiers::SHIFT);

        assert_eq!(messages.len(), 2);
        assert!(matches!(
            &messages[0],
            Message::OpenNotificationLink {
                host_id: actual_host,
                notification_id: actual_notification,
            } if actual_host == &host_id && actual_notification == &notification_id
        ));
        assert!(matches!(
            &messages[1],
            Message::OpenSession {
                host_id: actual_host,
                session_id: actual_session,
            } if actual_host == &host_id && actual_session == &session_id
        ));
    }

    fn assert_select_tab(messages: &[Message], expected: RightTab) {
        assert_eq!(messages.len(), 1);
        assert!(matches!(
            &messages[0],
            Message::SelectTab(actual) if *actual == expected
        ));
    }

    fn app_with_project_selection() -> PohunekApp {
        let host_id = HostId::new("local");
        let project = test_project();
        let mut host = test_host();
        host.projects.insert(project.id.clone(), project.clone());

        let mut app = app_without_selection();
        app.workspace.hosts.insert(host_id.clone(), host);
        app.ui_state.selection = Some(Selection::Project {
            host_id: host_id.clone(),
            project_id: project.id,
        });
        app.workspace.selection.clone_from(&app.ui_state.selection);
        app
    }

    fn app_with_live_notification(
        host_id: &HostId,
        session_id: &SessionId,
        notification_id: &NotificationId,
    ) -> PohunekApp {
        let mut host = test_host();
        host.sessions
            .insert(session_id.0.clone(), test_session(session_id));
        host.notifications.insert(
            notification_id.0.clone(),
            test_notification(notification_id, session_id),
        );

        let mut app = app_without_selection();
        app.workspace.hosts.insert(host_id.clone(), host);
        app
    }

    fn app_without_selection() -> PohunekApp {
        PohunekApp {
            workspace: pohunek_gui_core::Workspace::default(),
            config: Err("test config is intentionally absent".to_owned()),
            keymap: KeyMap::default(),
            hosts: Vec::new(),
            ui_state: pohunek_gui_core::UiState::default(),
            start: StartForm::default(),
            assistant: AssistantForm::default(),
            prompt_editor: iced::widget::text_editor::Content::new(),
            assistant_editor: iced::widget::text_editor::Content::new(),
            template_recipe: Option::<TemplateRecipe>::None,
            modal: ModalView::None,
            activity_filter: None,
            notification_filter: NotificationFilter::default(),
            inbox_scope: NotificationScope::default(),
            inbox_view: InboxView::default(),
            inbox_cursor: None,
            inbox_details_expanded: false,
            metadata_edit: MetadataEdit::default(),
            rename_edit: String::new(),
            project_edit: ProjectEdit::default(),
            selected_action: None,
            project_filters: BTreeMap::new(),
            last_session_click: None,
            state_dir: None,
            status: None,
            notified_intents: 0,
            blocked_cycle_index: 0,
        }
    }

    fn test_host() -> pohunek_gui_core::HostView {
        pohunek_gui_core::HostView {
            conn: ConnState::Connected,
            health: None,
            sessions: BTreeMap::new(),
            projects: BTreeMap::new(),
            project_details: BTreeMap::new(),
            notifications: BTreeMap::new(),
            prompt: pohunek_gui_core::PromptState::default(),
            provider: pohunek_gui_core::ProviderState::default(),
            last_agent_state: None,
            last_error: None,
        }
    }

    fn test_project() -> ProjectInfo {
        ProjectInfo {
            id: "p-1".to_owned(),
            label: "Project".to_owned(),
            repo_root: PathBuf::from("/tmp/project"),
            git_common_dir: PathBuf::from("/tmp/project/.git"),
            origin_url: None,
            default_base_branch: None,
            source: ProjectSource::Manual,
            is_bare: false,
            added_at: "2026-07-06T00:00:00Z".to_owned(),
            last_used_at: "2026-07-06T00:00:00Z".to_owned(),
        }
    }

    fn test_session(session_id: &SessionId) -> SessionInfo {
        SessionInfo {
            id: session_id.clone(),
            name: None,
            agent: "codex".to_owned(),
            agent_base: AgentKind::Codex,
            cwd: PathBuf::from("/tmp/project"),
            pid: 42,
            cols: 80,
            rows: 24,
            state: SessionState::Running,
            state_source: StateSource::Process,
            activity: None,
            active_agent: None,
            active_agent_base: None,
            active_agent_session_id: None,
            active_agent_session_path: None,
            native_session_id: Some("native-1".to_owned()),
            native_session_path: None,
            project_id: Some("p-1".to_owned()),
            project_label: Some("Project".to_owned()),
            is_linked_worktree: Some(false),
            repo: Some(PathBuf::from("/tmp/project")),
            branch: Some("main".to_owned()),
            worktree_path: Some(PathBuf::from("/tmp/project")),
            metadata: BTreeMap::new(),
            warnings: Vec::new(),
            created_at: "2026-07-06T00:00:00Z".to_owned(),
            updated_at: "2026-07-06T00:00:00Z".to_owned(),
            exit_code: None,
        }
    }

    fn test_notification(
        notification_id: &NotificationId,
        session_id: &SessionId,
    ) -> NotificationRecord {
        NotificationRecord {
            id: notification_id.clone(),
            source: NotificationSource {
                provider: "test".to_owned(),
                provider_event: "event".to_owned(),
                host_local_source_id: "source-1".to_owned(),
            },
            kind: NotificationKind::AgentBlocked,
            severity: NotificationSeverity::Warning,
            status: NotificationStatus::Unread,
            title: "Blocked".to_owned(),
            body: "Needs attention".to_owned(),
            metadata: BTreeMap::new(),
            created_at: "2026-07-06T00:00:00Z".to_owned(),
            session_id: Some(session_id.clone()),
            agent_kind: Some(AgentKind::Codex),
            source_id: None,
            dedupe_key: None,
            project_id: Some("p-1".to_owned()),
            read_at: None,
            acked_at: None,
            archived_at: None,
            deleted_at: None,
            superseded_by: None,
        }
    }

    fn test_linear_issue(identifier: &str, title: &str) -> providers::linear::LinearIssue {
        providers::linear::LinearIssue {
            id: format!("{identifier}-opaque"),
            identifier: identifier.to_owned(),
            title: title.to_owned(),
            body: "Issue body".to_owned(),
            branch: format!("feature/{}", identifier.to_lowercase()),
            url: format!("https://linear.example/{identifier}"),
            state: None,
            state_type: None,
            assignee: None,
            updated_at: None,
        }
    }

    fn test_github_pull_request(
        number: u64,
        title: &str,
        head_ref_name: &str,
    ) -> providers::github::GitHubPullRequest {
        providers::github::GitHubPullRequest::new(
            number,
            title,
            "",
            head_ref_name,
            format!("https://github.example/repo/pull/{number}"),
        )
    }

    fn test_github_issue(number: u64, title: &str) -> providers::github::GitHubIssue {
        providers::github::GitHubIssue {
            number,
            title: title.to_owned(),
            body: String::new(),
            url: format!("https://github.example/repo/issues/{number}"),
            branch: None,
        }
    }

    fn test_github_scope() -> GitHubProviderScope {
        GitHubProviderScope::new("p-1", "/tmp/project")
    }
}
