//! The Iced `Message` enum, modal routing, and the small UI form/edit state it drives.

use std::path::PathBuf;

use iced::keyboard::{Key, Modifiers};
use iced::widget::text_editor;
use iced::Size;
use pohunek_gui_core::assistant::Intent as AssistantIntent;
use pohunek_gui_core::{
    DomainEvent as CoreEvent, HostConfig, HostId, NotificationScope, ReviewLineTarget, RightTab,
    TreeNodeId,
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
    /// The effective keyboard shortcut table.
    Keymap,
    /// The selected provider item (PR/issue) detail and launch dialog.
    ProviderItem,
    /// The inbox: notification list, or one message's detail.
    Inbox,
    /// The Review tab's "Dispatch as session…" confirmation.
    DispatchReview,
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

/// Compiled base agent kinds, used as the picker fallback when a host's
/// `supported_agents` (seeded from `host.inspect`) is unavailable — e.g. the
/// snapshot has not loaded yet, or an older daemon does not answer the
/// method. Mirrors the daemon's own base-kind list
/// (`crates/daemon/src/capabilities.rs`).
pub(crate) const BASE_AGENT_KINDS: [&str; 3] = ["shell", "codex", "claude"];

/// State for the intent-driven "Start session" panel. The project, repo, cwd and
/// terminal size are derived from the selected project and config rather than
/// typed; only the runtime, an optional initial input and (under Advanced) branch
/// overrides are operator-supplied.
#[derive(Debug, Clone)]
pub(crate) struct StartForm {
    /// Wire agent string (`session new --agent`), one of the selected host's
    /// `supported_agents` or a `BASE_AGENT_KINDS` fallback.
    pub(crate) agent: String,
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
            agent: "codex".to_owned(),
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

/// Direction for keyboard-driven list movement.
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
    /// Switch the right pane's persistent tab. Only dispatched for a tab the
    /// operator can actually reach (tabs 2-4 render with no `on_press` when the
    /// current selection has no project scope).
    SelectTab(RightTab),
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
    ForkSelectedSession,
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
    /// Copy arbitrary text (e.g. a Linear/GitHub item's branch name) to the
    /// system clipboard.
    CopyText(String),
    /// Open a provider item's URL in the OS browser (argv-spawned, never a
    /// shell — see `attach::spawn_open_url`).
    OpenUrl(String),
    /// Remove a single pohunek-owned worktree by path.
    RemoveWorktree(PathBuf),
    /// Move the keyboard cursor in the active list, wrapping at either end.
    MoveListSelection(ListDirection),
    /// Focus the active provider tab's local search box.
    FocusProviderSearch,
    SelectAction(String),
    /// Pick a Linear filter and immediately fetch its issues.
    SelectLinearFilter(String),
    /// Pick a GitHub pull request filter and immediately fetch.
    SelectGitHubFilter(String),
    /// Edit the Linear provider search box.
    LinearSearchChanged {
        host_id: HostId,
        value: String,
    },
    /// Edit the GitHub provider search box.
    GitHubSearchChanged {
        host_id: HostId,
        value: String,
    },
    FetchLinearIssues,
    FetchGitHubPullRequests,
    FetchGitHubIssues,
    FetchGitHubPullRequestStatus,
    LaunchLinearIssue,
    LaunchGitHubPullRequest,
    CoreCommandCompleted(Result<CoreEvent, String>),
    AttachSpawned(Result<(), String>),
    NotificationSent(Result<(), String>),
    UrlOpened(Result<(), String>),
    WindowResized(Size),
    UiStateSaved(Result<(), String>),
    /// A key press Iced did not already hand to a focused widget; routed by
    /// `crate::keyboard::route_key_press` into zero or more of the messages
    /// above.
    KeyPressed {
        key: Key,
        modifiers: Modifiers,
    },
    /// The `b` shortcut: select the next blocked agent, wrapping around.
    CycleBlockedAgent,
    /// Open the Review tab for a session's worktree diff against its base.
    OpenSessionReview {
        host_id: HostId,
        session_id: SessionId,
    },
    /// Open the Review tab for a GitHub pull request diff (matching
    /// `OpenGitHubPullRequest`'s shape: the host resolves from the current
    /// selection, the same way opening the item modal already does).
    OpenPullRequestReview {
        number: u64,
    },
    /// Re-fetch the Review tab's diff for its current source.
    RefreshReviewDiff,
    /// Select a file row in the Review tab's file list.
    SelectReviewFile(usize),
    /// Select one diff line in the Review tab (mouse click on a line).
    SelectReviewLine(ReviewLineTarget),
    /// Open the inline comment editor for the currently selected line.
    BeginReviewComment,
    /// Open the inline comment editor to edit an existing comment.
    BeginEditReviewComment(usize),
    /// Edit the open comment editor's draft text.
    ReviewCommentDraftChanged(String),
    /// Save the open comment editor (add or edit a comment) and persist it.
    SaveReviewComment,
    /// Close the comment editor without saving.
    CancelReviewComment,
    /// Remove a comment from the active review and persist it.
    RemoveReviewComment(usize),
    /// Open the "Dispatch as session…" modal for the active review.
    OpenReviewDispatchModal,
    /// Pick the agent the dispatched session will run, overriding the source
    /// session's own profile (reuses the Start modal's agent picker).
    DispatchAgentSelected(String),
    /// Confirm dispatching the active review as a new same-worktree session.
    ConfirmReviewDispatch,
}

#[derive(Debug, Clone)]
pub(crate) struct DiscoveryResult {
    pub(crate) hosts: Vec<HostConfig>,
    pub(crate) warning: Option<String>,
}
