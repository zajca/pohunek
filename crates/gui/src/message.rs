//! Native GUI messages, modal routing, and form state.

// Rust guideline compliant 2026-08-12

use iced::keyboard::{Key, Modifiers};
use iced::widget::text_editor;
use iced::Size;
use pohunek_gui_core::assistant::Intent as AssistantIntent;
use pohunek_gui_core::{
    DomainEvent as CoreEvent, HostConfig, HostId, NotificationScope, ProjectRef,
};
use protocol::{NotificationId, NotificationKind, SessionId};

pub(crate) const BLANK_TEMPLATE_LABEL: &str = "— blank —";
pub(crate) const ASSISTANT_AUTO_AGENT_LABEL: &str = "Auto";
pub(crate) const PROJECT_PLACEHOLDER_LABEL: &str = "Choose a project";

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
    Hosts,
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
    /// Target project, which also decides the host the session runs on.
    pub(crate) project: Option<ProjectRef>,
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
            project: None,
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
    /// Target project, which also decides the host the assistant runs on.
    pub(crate) project: Option<ProjectRef>,
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
            project: None,
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

/// A keyboard-focusable field in a launch form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FormField {
    StartProject,
    StartAgent,
    StartTemplate,
    StartName,
    StartPrompt,
    StartBranch,
    StartBaseBranch,
    AssistantProject,
    AssistantIntent,
    AssistantAgent,
    AssistantRequest,
    AssistantBranch,
    AssistantBaseBranch,
}

/// State of an expanded keyboard-controlled select field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FormSelect {
    pub(crate) field: FormField,
    pub(crate) cursor: usize,
}

#[derive(Debug, Clone)]
pub(crate) enum Message {
    Core(CoreEvent),
    HostsDiscovered(DiscoveryResult),
    OpenInbox,
    OpenHostsModal,
    SetProjectFilter(Option<ProjectRef>),
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
    StartProjectSelected(ProjectRef),
    StartAgentSelected(String),
    StartTemplateSelected(String),
    /// Template resolution requested under `generation`; applied only while
    /// that is still the newest template request.
    TemplateResolved {
        generation: u64,
        result: Result<ResolvedTemplate, String>,
    },
    PromptEdited(text_editor::Action),
    AssistantRequestEdited(text_editor::Action),
    AssistantProjectSelected(ProjectRef),
    AssistantIntentSelected(AssistantIntent),
    AssistantAgentSelected(String),
    ToggleAssistantAdvanced,
    AssistantBranchChanged(String),
    AssistantBaseBranchChanged(String),
    AssistantNoSnapshotToggled(bool),
    AssistantDegradedToggled(bool),
    ToggleFormSelect(FormField),
    MoveFormSelect(ListDirection),
    ConfirmFormSelect,
    CloseFormSelect,
    ChooseFormSelect {
        field: FormField,
        index: usize,
    },
    TraverseFormFocus {
        focused: Option<FormField>,
        direction: ListDirection,
    },
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
    /// Attach outcome: an optional warning on success, or the failure text.
    AttachSpawned(Result<Option<String>, String>),
    NotificationSent(crate::notify::NotificationOutcome),
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
