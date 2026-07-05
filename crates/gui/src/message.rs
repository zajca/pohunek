//! The Iced `Message` enum, modal routing, and the small UI form/edit state it drives.

use std::path::PathBuf;

use iced::widget::text_editor;
use iced::Size;
use pohunek_gui_core::assistant::Intent as AssistantIntent;
use pohunek_gui_core::{
    HostConfig, HostId, Message as CoreMessage, NotificationScope, ProviderPanel, TreeNodeId,
};
use protocol::{AgentActivity, NotificationId, SessionId};

// Sentinel option in the Start template picker meaning "no template, blank session".
pub(crate) const BLANK_TEMPLATE_LABEL: &str = "— blank —";

pub(crate) const ASSISTANT_AUTO_AGENT_LABEL: &str = "Auto";

/// Which overlay modal is open.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModalView {
    #[default]
    None,
    /// The "Start a session" dialog.
    Start,
    /// The "Start assistant" dialog.
    Assistant,
    /// The selected provider item (PR/issue) detail and launch dialog.
    ProviderItem,
    /// The inbox: notification list, or one message's detail.
    Inbox,
}

/// Which layer of the inbox modal is showing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum InboxView {
    /// The notification list, optionally scoped and host-filtered.
    #[default]
    List,
    /// One notification's detail, auto-marked read on entry.
    Message {
        host_id: HostId,
        notification_id: NotificationId,
    },
}

/// Launch recipe resolved from a selected template (a `None`-provider action).
#[derive(Debug, Clone)]
pub(crate) struct TemplateRecipe {
    pub(crate) agent: String,
    pub(crate) branch: Option<String>,
    pub(crate) base_branch: Option<String>,
}

/// Rendered template plus its recipe, produced by resolving a template action.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedTemplate {
    pub(crate) rendered: String,
    pub(crate) recipe: TemplateRecipe,
}

/// Runtime the operator can launch from the GUI. Backed by the protocol
/// base-kind wire strings; rendered in a `pick_list` instead of being typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentChoice {
    Shell,
    Codex,
    Claude,
}

impl AgentChoice {
    /// Selectable runtimes, in display order.
    pub(crate) const ALL: [Self; 3] = [Self::Shell, Self::Codex, Self::Claude];

    /// Wire string passed verbatim to `session new --agent`.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }
}

impl std::fmt::Display for AgentChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// State for the intent-driven "Start session" panel. The project, repo, cwd and
/// terminal size are derived from the selected project and config rather than
/// typed; only the runtime, an optional initial input and (under Advanced) branch
/// overrides are operator-supplied.
#[derive(Debug, Clone)]
pub(crate) struct StartForm {
    pub(crate) agent: AgentChoice,
    /// Owner-set display name to stamp on the session, shared by the manual Start
    /// modal and the provider-launch modal (only one is open at a time). Empty
    /// means an unnamed session shown by its id.
    pub(crate) name: String,
    /// Selected prompt template (a `None`-provider action name); `None` means a
    /// blank session whose input is whatever is typed into the prompt editor.
    pub(crate) template: Option<String>,
    pub(crate) show_advanced: bool,
    pub(crate) branch: String,
    pub(crate) base_branch: String,
}

impl Default for StartForm {
    fn default() -> Self {
        Self {
            agent: AgentChoice::Codex,
            name: String::new(),
            template: None,
            show_advanced: false,
            branch: String::new(),
            base_branch: String::new(),
        }
    }
}

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

impl AgentChoice {
    /// Maps a wire agent string to a selectable choice, defaulting to Codex.
    pub(crate) fn from_wire(value: &str) -> Self {
        match value {
            "shell" => Self::Shell,
            "claude" => Self::Claude,
            _ => Self::Codex,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct MetadataEdit {
    pub(crate) key: String,
    pub(crate) value: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ProjectEdit {
    pub(crate) path: String,
    pub(crate) name: String,
    pub(crate) base_branch: String,
    pub(crate) reference: String,
    pub(crate) rename_to: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotificationAction {
    Read,
    Acknowledge,
    Archive,
    Delete,
}

#[derive(Debug, Clone)]
pub(crate) enum Message {
    Core(CoreMessage),
    HostsDiscovered(DiscoveryResult),
    ToggleNode(TreeNodeId),
    FilterActivity(Option<AgentActivity>),
    OpenInbox,
    OpenHostInbox(HostId),
    /// Pick the inbox modal's `Needs action | All | Archived` scope.
    SetInboxScope(NotificationScope),
    FilterNotificationHost(Option<HostId>),
    /// Open a notification's message-detail layer; auto-marks it read.
    SelectNotification {
        host_id: HostId,
        notification_id: NotificationId,
    },
    /// Step the inbox modal's message-detail layer back to the list.
    InboxBack,
    /// Expand or collapse the message-detail layer's `> Details` section.
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
    OpenStartModal,
    OpenAssistantModal,
    CloseModal,
    StartAgentSelected(AgentChoice),
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
    /// Edit the rename buffer for the selected session.
    RenameEditChanged(String),
    /// Apply the rename buffer as the selected session's display name.
    RenameSession,
    /// Clear the selected session's display name.
    ClearSessionName,
    OpenLinearIssue(String),
    OpenGitHubPullRequest(u64),
    OpenGitHubIssue(u64),
    InspectSelectedSession,
    StopSelectedSession,
    /// Remove the selected session from the daemon, stopping it first if live.
    RemoveSelectedSession,
    MetadataKeyChanged(String),
    MetadataValueChanged(String),
    SetMetadata,
    ClearMetadata,
    ProjectPathChanged(String),
    ProjectNameChanged(String),
    ProjectBaseBranchChanged(String),
    ProjectRenameToChanged(String),
    AddProject,
    ShowProject,
    RenameProject,
    /// Copy a worktree's absolute path to the system clipboard.
    CopyWorktreePath(PathBuf),
    /// Remove a single pohunek-owned worktree by path.
    RemoveWorktree(PathBuf),
    SelectAction(String),
    SelectProviderPanel(ProviderPanel),
    /// Pick a Linear filter and immediately fetch its issues.
    SelectLinearFilter(String),
    /// Pick a GitHub pull request filter and immediately fetch.
    SelectGitHubFilter(String),
    FetchLinearIssues,
    FetchGitHubPullRequests,
    FetchGitHubIssues,
    FetchGitHubPullRequestStatus,
    LaunchLinearIssue,
    LaunchGitHubPullRequest,
    CoreCommandCompleted(Result<CoreMessage, String>),
    AttachSpawned(Result<(), String>),
    NotificationSent(Result<(), String>),
    WindowResized(Size),
    UiStateSaved(Result<(), String>),
}

#[derive(Debug, Clone)]
pub(crate) struct DiscoveryResult {
    pub(crate) hosts: Vec<HostConfig>,
    pub(crate) warning: Option<String>,
}
