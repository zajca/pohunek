//! Native GUI messages, modal routing, and form state.

// Rust guideline compliant 2026-08-12

use iced::keyboard::{Key, Modifiers};
use iced::widget::text_editor;
use iced::Size;
use pohunek_gui_core::assistant::Intent as AssistantIntent;
use pohunek_gui_core::{
    DomainEvent as CoreEvent, HostConfig, HostId, NotificationScope, TreeNodeId,
};
use protocol::{NotificationId, NotificationKind, SessionId};

pub(crate) const BLANK_TEMPLATE_LABEL: &str = "— blank —";
pub(crate) const ASSISTANT_AUTO_AGENT_LABEL: &str = "Auto";

/// Which overlay modal is open.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModalView {
    #[default]
    None,
    Start,
    Assistant,
    Session,
    ConfirmDeleteSession,
    Keymap,
    Inbox,
}

/// Which layer of the inbox modal is showing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum InboxView {
    #[default]
    List,
    Message {
        host_id: HostId,
        notification_id: NotificationId,
    },
}

/// Launch recipe resolved from a static project template.
#[derive(Debug, Clone)]
pub(crate) struct TemplateRecipe {
    pub(crate) agent: String,
    pub(crate) branch: Option<String>,
    pub(crate) base_branch: Option<String>,
}

/// Rendered template plus its launch recipe.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedTemplate {
    pub(crate) rendered: String,
    pub(crate) recipe: TemplateRecipe,
}

/// User-editable fields in the session-start modal.
#[derive(Debug, Clone)]
pub(crate) struct StartForm {
    pub(crate) agent: String,
    pub(crate) name: String,
    pub(crate) template: Option<String>,
    pub(crate) show_advanced: bool,
    pub(crate) branch: String,
    pub(crate) base_branch: String,
}

impl Default for StartForm {
    fn default() -> Self {
        Self {
            agent: "codex".to_owned(),
            name: String::new(),
            template: None,
            show_advanced: false,
            branch: String::new(),
            base_branch: String::new(),
        }
    }
}

/// User-editable fields in the assistant-start modal.
#[derive(Debug, Clone)]
pub(crate) struct AssistantForm {
    pub(crate) intent: AssistantIntent,
    pub(crate) agent: Option<String>,
    pub(crate) show_advanced: bool,
    pub(crate) branch: String,
    pub(crate) base_branch: String,
    pub(crate) no_snapshot: bool,
    pub(crate) degraded: bool,
}

impl Default for AssistantForm {
    fn default() -> Self {
        Self {
            intent: AssistantIntent::Help,
            agent: None,
            show_advanced: false,
            branch: String::new(),
            base_branch: String::new(),
            no_snapshot: false,
            degraded: false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct MetadataEdit {
    pub(crate) key: String,
    pub(crate) value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotificationAction {
    Read,
    Acknowledge,
    Archive,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListDirection {
    Up,
    Down,
}

#[derive(Debug, Clone)]
pub(crate) enum Message {
    Core(CoreEvent),
    HostsDiscovered(DiscoveryResult),
    ToggleNode(TreeNodeId),
    OpenInbox,
    OpenHostInbox(HostId),
    SetInboxScope(NotificationScope),
    FilterNotificationHost(Option<HostId>),
    SelectNotification {
        host_id: HostId,
        notification_id: NotificationId,
    },
    InboxBack,
    ToggleInboxDetails,
    OpenNotificationLink {
        host_id: HostId,
        notification_id: NotificationId,
    },
    ActOnNotification {
        host_id: HostId,
        notification_id: NotificationId,
        action: NotificationAction,
    },
    SelectSession {
        host_id: HostId,
        session_id: SessionId,
    },
    SelectProject {
        host_id: HostId,
        project_id: String,
    },
    OpenSession {
        host_id: HostId,
        session_id: SessionId,
    },
    StopSession {
        host_id: HostId,
        session_id: SessionId,
    },
    RequestDeleteSession {
        host_id: HostId,
        session_id: SessionId,
    },
    ConfirmDeleteSession,
    OpenStartModal,
    OpenAssistantModal,
    OpenKeymapModal,
    CloseModal,
    StartAgentSelected(String),
    StartTemplateSelected(String),
    TemplateResolved(Result<ResolvedTemplate, String>),
    PromptEdited(text_editor::Action),
    AssistantRequestEdited(text_editor::Action),
    AssistantIntentSelected(AssistantIntent),
    AssistantAgentSelected(String),
    ToggleAssistantAdvanced,
    AssistantBranchChanged(String),
    AssistantBaseBranchChanged(String),
    AssistantNoSnapshotToggled(bool),
    AssistantDegradedToggled(bool),
    LaunchAssistant,
    ToggleStartAdvanced,
    StartBranchChanged(String),
    StartBaseBranchChanged(String),
    StartNameChanged(String),
    CreateSession,
    RenameEditChanged(String),
    RenameSession,
    ClearSessionName,
    InspectSelectedSession,
    ReadSelectedSessionScreen,
    ReadSelectedSessionOutput,
    WaitForSelectedSession,
    ForkSelectedSession,
    MetadataKeyChanged(String),
    MetadataValueChanged(String),
    SetMetadata,
    ClearMetadata,
    LoadNotificationPolicy(HostId),
    SetNotificationPolicyKind {
        host_id: HostId,
        provider: Option<String>,
        kind: NotificationKind,
        enabled: bool,
    },
    SaveNotificationPolicy(HostId),
    MoveListSelection(ListDirection),
    CoreCommandCompleted(Result<CoreEvent, String>),
    AttachSpawned(Result<(), String>),
    NotificationSent(Result<(), String>),
    WindowResized(Size),
    UiStateSaved(Result<(), String>),
    KeyPressed {
        key: Key,
        modifiers: Modifiers,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct DiscoveryResult {
    pub(crate) hosts: Vec<HostConfig>,
    pub(crate) warning: Option<String>,
}
