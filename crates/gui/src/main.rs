//! Native Iced shell for the pohunek control plane.

// Rust guideline compliant 2026-06-30
#![forbid(unsafe_code)]

mod runtime;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use iced::widget::{
    button, center, checkbox, column, container, mouse_area, opaque, pick_list, row, scrollable,
    stack, text, text_editor, text_input,
};
use iced::{window, Background, Center, Color, Element, Fill, Size, Subscription, Task, Theme};
use pohunek_gui_core::assistant::{
    AssistantPaths, Intent as AssistantIntent, LaunchParams as AssistantLaunchParams,
};
use pohunek_gui_core::{
    add_project_with_options, assistant as assistant_core, create_session_with_options,
    default_state_dir, delete_notification_with_options, discover_hosts,
    inspect_session_with_options, launch_provider_item_with_options,
    list_project_actions_with_options, preview_action_prompt, providers,
    remove_session_with_options, remove_worktree_with_options, rename_project_with_options,
    rename_session_with_options, resolve_project_action_with_options, resume_session_with_options,
    session_link_metadata, session_metadata_rows, set_session_metadata_with_options,
    show_project_with_options, spawn_attach_command, stop_session_with_options,
    update_notification_with_options, AttachCommandSpawner, AttachTemplateValues, ConnState,
    ConnectionOptions, GitHubProviderScope, GitHubPullRequestStatusKey, HostConfig, HostId,
    Message as CoreMessage, NotificationFilter, NotificationIntent, ProviderLaunchItem,
    ProviderLaunchParams, ProviderOperation, ProviderPanel, ProviderRequestId, Selection,
    SessionLinkKind, SessionLinkProvider, Toast, TreeNodeId, UiState, WindowSize, Workspace,
};
use protocol::{
    AgentActivity, NotificationDeleteParams, NotificationId, NotificationKind, NotificationRecord,
    NotificationSeverity, NotificationStatus, NotificationUpdateParams, ProjectActionParams,
    ProjectActionsParams, ProjectAddParams, ProjectInfo, ProjectRenameParams, ProjectShowParams,
    ProjectWorktree, ProviderKind, SessionId, SessionInfo, SessionNewParams, SessionRenameParams,
    SessionSetMetadataParams, WorktreeRemoveParams,
};
use serde::Deserialize;
use thiserror::Error;

// 80x24 is the traditional terminal size expected by many CLI tools.
const DEFAULT_TERMINAL_COLS: u16 = 80;
const DEFAULT_TERMINAL_ROWS: u16 = 24;
// notify-send is the freedesktop notification CLI available on target Linux desktops.
const DEFAULT_NOTIFICATION_COMMAND: &str = "notify-send";
// Sentinel option in the Start template picker meaning "no template, blank session".
const BLANK_TEMPLATE_LABEL: &str = "— blank —";
const ASSISTANT_AUTO_AGENT_LABEL: &str = "Auto";
// A second click on the same session within this window counts as a double-click
// and opens the session in a terminal (matching the desktop double-click idiom).
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);
// Wayland clients discover their compositor through this standard variable.
const WAYLAND_DISPLAY_ENV: &str = "WAYLAND_DISPLAY";
// X11 clients use this standard variable; seeing it without Wayland gives a
// clearer error than letting the window backend fail later.
const X11_DISPLAY_ENV: &str = "DISPLAY";
// Calendar conversion offset from the civil-date algorithm's day zero to Unix
// epoch; changing it would make notification age labels wrong for every row.
const UNIX_EPOCH_DAY_OFFSET: i64 = 719_468;
// Gregorian 400-year era length used by the civil date conversion below.
const DAYS_PER_ERA: i64 = 146_097;
// Calendar years in one Gregorian era.
const YEARS_PER_ERA: i64 = 400;
// March-based month arithmetic used by the civil date conversion.
const MARCH_BASED_MONTH_OFFSET: i64 = 9;
// Month numerator from Howard Hinnant's days-from-civil algorithm.
const MONTH_DAY_NUMERATOR: i64 = 153;
// Notification age labels are intentionally coarse; these named thresholds keep
// row text short and prevent timestamp math from scattering UI constants.
const SECONDS_PER_MINUTE: u64 = 60;
const SECONDS_PER_HOUR: u64 = 60 * SECONDS_PER_MINUTE;
const SECONDS_PER_DAY: u64 = 24 * SECONDS_PER_HOUR;
const SECONDS_PER_WEEK: u64 = 7 * SECONDS_PER_DAY;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), StartupError> {
    validate_wayland_environment()?;
    let boot = BootState::load();
    let initial_window_size = boot.ui_state.window_size;
    iced::application(move || PohunekApp::boot(boot.clone()), update, view)
        .subscription(subscription)
        .theme(theme)
        .window_size((
            window_dimension_to_f32(initial_window_size.width),
            window_dimension_to_f32(initial_window_size.height),
        ))
        .run()?;
    Ok(())
}

#[derive(Debug, Error)]
enum StartupError {
    #[error(transparent)]
    Display(#[from] DisplayServerError),
    #[error(transparent)]
    Iced(#[from] iced::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayServerError {
    MissingWayland,
    X11WithoutWayland,
}

impl std::fmt::Display for DisplayServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingWayland => write!(
                f,
                "pohunek-gui is Wayland-only; set `{WAYLAND_DISPLAY_ENV}` to a Wayland display before starting it"
            ),
            Self::X11WithoutWayland => write!(
                f,
                "pohunek-gui is Wayland-only; {X11_DISPLAY_ENV} is set, but {WAYLAND_DISPLAY_ENV} is missing or empty. X11 is not supported"
            ),
        }
    }
}

impl std::error::Error for DisplayServerError {}

fn validate_wayland_environment() -> Result<(), DisplayServerError> {
    let wayland_display = std::env::var_os(WAYLAND_DISPLAY_ENV);
    let x11_display = std::env::var_os(X11_DISPLAY_ENV);
    validate_wayland_display(wayland_display.as_deref(), x11_display.as_deref())
}

fn validate_wayland_display(
    wayland_display: Option<&OsStr>,
    x11_display: Option<&OsStr>,
) -> Result<(), DisplayServerError> {
    if has_display_value(wayland_display) {
        Ok(())
    } else if has_display_value(x11_display) {
        Err(DisplayServerError::X11WithoutWayland)
    } else {
        Err(DisplayServerError::MissingWayland)
    }
}

fn has_display_value(value: Option<&OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

#[derive(Debug, Clone)]
struct BootState {
    ui_state: UiState,
    state_dir: Option<PathBuf>,
    status: Option<String>,
}

impl BootState {
    fn load() -> Self {
        match default_state_dir() {
            Ok(state_dir) => match UiState::load_from_dir(&state_dir) {
                Ok(ui_state) => Self {
                    ui_state,
                    state_dir: Some(state_dir),
                    status: None,
                },
                Err(err) => Self {
                    ui_state: UiState::default(),
                    state_dir: Some(state_dir),
                    status: Some(err.to_string()),
                },
            },
            Err(err) => Self {
                ui_state: UiState::default(),
                state_dir: None,
                status: Some(err.to_string()),
            },
        }
    }
}

// Not `Clone`: `text_editor::Content` (the editable prompt buffer) is not
// clonable, and the application state is owned by Iced and never cloned.
#[derive(Debug)]
struct PohunekApp {
    workspace: Workspace,
    config: Result<AppConfig, String>,
    hosts: Vec<HostConfig>,
    ui_state: UiState,
    start: StartForm,
    assistant: AssistantForm,
    /// Editable session input / rendered prompt buffer shown in the Start modal.
    prompt_editor: text_editor::Content,
    /// Editable request buffer shown in the Assistant modal.
    assistant_editor: text_editor::Content,
    /// Resolved recipe (agent/branch) for the selected template; `None` when a
    /// blank session or while a template is still resolving.
    template_recipe: Option<TemplateRecipe>,
    /// Which modal, if any, is currently open over the workspace.
    modal: ModalView,
    /// Active activity filter for the agents monitor; `None` shows all agents.
    activity_filter: Option<AgentActivity>,
    /// Active inbox filters; `None` fields do not constrain the notification list.
    notification_filter: NotificationFilter,
    /// Whether the detail pane is showing the inbox list instead of start work.
    inbox_open: bool,
    metadata_edit: MetadataEdit,
    /// Edit buffer for renaming the selected session's display name.
    rename_edit: String,
    project_edit: ProjectEdit,
    /// Action chosen in the provider browser for launching the selected item.
    selected_action: Option<String>,
    /// Per-project provider filters read from each project's in-repo
    /// `.pohunek/providers.toml`, keyed by repository root. Populated lazily on
    /// project selection / provider fetch; absent entries fall back to the host
    /// (`gui.toml`) and built-in layers.
    project_filters: BTreeMap<PathBuf, providers::filters::ProviderFilterSet>,
    /// Last session row click (host, session, instant), used to detect a
    /// double-click that opens the session in a terminal.
    last_session_click: Option<(HostId, SessionId, Instant)>,
    state_dir: Option<PathBuf>,
    status: Option<String>,
    notified_intents: usize,
}

/// Which overlay modal is open.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum ModalView {
    #[default]
    None,
    /// The "Start a session" dialog.
    Start,
    /// The "Start assistant" dialog.
    Assistant,
    /// The selected provider item (PR/issue) detail and launch dialog.
    ProviderItem,
}

/// Launch recipe resolved from a selected template (a `None`-provider action).
#[derive(Debug, Clone)]
struct TemplateRecipe {
    agent: String,
    branch: Option<String>,
    base_branch: Option<String>,
}

/// Rendered template plus its recipe, produced by resolving a template action.
#[derive(Debug, Clone)]
struct ResolvedTemplate {
    rendered: String,
    recipe: TemplateRecipe,
}

impl PohunekApp {
    fn boot(boot: BootState) -> (Self, Task<Message>) {
        let config = AppConfig::load().map_err(|err| err.to_string());
        let task = match &config {
            Ok(config) => discover_hosts_task(config),
            Err(_) => Task::none(),
        };
        let mut workspace = Workspace::default();
        workspace.selection.clone_from(&boot.ui_state.selection);
        (
            Self {
                workspace,
                config,
                hosts: Vec::new(),
                ui_state: boot.ui_state,
                start: StartForm::default(),
                assistant: AssistantForm::default(),
                prompt_editor: text_editor::Content::new(),
                assistant_editor: text_editor::Content::new(),
                template_recipe: None,
                modal: ModalView::None,
                activity_filter: None,
                notification_filter: NotificationFilter::default(),
                inbox_open: false,
                metadata_edit: MetadataEdit::default(),
                rename_edit: String::new(),
                project_edit: ProjectEdit::default(),
                selected_action: None,
                project_filters: BTreeMap::new(),
                last_session_click: None,
                state_dir: boot.state_dir,
                status: boot.status,
                notified_intents: 0,
            },
            task,
        )
    }

    fn attach_values(
        &self,
        host_id: &HostId,
        session_id: &SessionId,
    ) -> Result<(String, AttachTemplateValues), String> {
        let config = self.config.as_ref().map_err(Clone::clone)?;
        let host = self
            .hosts
            .iter()
            .find(|host| &host.id == host_id)
            .ok_or_else(|| format!("unknown host `{host_id}`"))?;
        Ok((
            config.attach_command.clone(),
            AttachTemplateValues {
                bin: config.pohunek_bin.clone(),
                host: host.attach_host().to_owned(),
                id: session_id.0.clone(),
            },
        ))
    }
}

/// Runtime the operator can launch from the GUI. Backed by the protocol
/// base-kind wire strings; rendered in a `pick_list` instead of being typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentChoice {
    Shell,
    Codex,
    Claude,
}

impl AgentChoice {
    /// Selectable runtimes, in display order.
    const ALL: [Self; 3] = [Self::Shell, Self::Codex, Self::Claude];

    /// Wire string passed verbatim to `session new --agent`.
    const fn as_str(self) -> &'static str {
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
struct StartForm {
    agent: AgentChoice,
    /// Owner-set display name to stamp on the session, shared by the manual Start
    /// modal and the provider-launch modal (only one is open at a time). Empty
    /// means an unnamed session shown by its id.
    name: String,
    /// Selected prompt template (a `None`-provider action name); `None` means a
    /// blank session whose input is whatever is typed into the prompt editor.
    template: Option<String>,
    show_advanced: bool,
    branch: String,
    base_branch: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalSize {
    cols: u16,
    rows: u16,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self {
            cols: DEFAULT_TERMINAL_COLS,
            rows: DEFAULT_TERMINAL_ROWS,
        }
    }
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
struct AssistantForm {
    intent: AssistantIntent,
    agent: Option<String>,
    show_advanced: bool,
    branch: String,
    base_branch: String,
    no_snapshot: bool,
    degraded: bool,
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
    fn from_wire(value: &str) -> Self {
        match value {
            "shell" => Self::Shell,
            "claude" => Self::Claude,
            _ => Self::Codex,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct MetadataEdit {
    key: String,
    value: String,
}

#[derive(Debug, Clone, Default)]
struct ProjectEdit {
    path: String,
    name: String,
    base_branch: String,
    reference: String,
    rename_to: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotificationAction {
    Read,
    Acknowledge,
    Archive,
    Delete,
}

#[derive(Debug, Clone)]
enum Message {
    Core(CoreMessage),
    HostsDiscovered(DiscoveryResult),
    ToggleNode(TreeNodeId),
    FilterActivity(Option<AgentActivity>),
    OpenInbox,
    OpenHostInbox(HostId),
    FilterNotificationStatus(Option<NotificationStatus>),
    FilterNotificationSeverity(Option<NotificationSeverity>),
    FilterNotificationKind(Option<NotificationKind>),
    FilterNotificationProvider(Option<String>),
    FilterNotificationHost(Option<HostId>),
    ClearNotificationFilters,
    SelectNotification {
        host_id: HostId,
        notification_id: NotificationId,
    },
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
struct DiscoveryResult {
    hosts: Vec<HostConfig>,
    warning: Option<String>,
}

#[expect(
    clippy::too_many_lines,
    reason = "Iced update centralizes shell messages and delegates domain transitions to gui-core"
)]
fn update(app: &mut PohunekApp, message: Message) -> Task<Message> {
    let mut tasks = Vec::new();
    match message {
        Message::Core(message) => {
            app.workspace.apply(message);
            tasks.push(notification_tasks(app));
        }
        Message::HostsDiscovered(result) => {
            app.hosts = result.hosts;
            app.status = result.warning;
        }
        Message::ToggleNode(node) => {
            if app.ui_state.expanded_nodes.contains(&node) {
                app.ui_state.expanded_nodes.remove(&node);
            } else {
                app.ui_state.expanded_nodes.insert(node);
            }
            tasks.push(save_ui_state_task(app));
        }
        Message::FilterActivity(activity) => app.activity_filter = activity,
        Message::OpenInbox => {
            app.inbox_open = true;
            app.notification_filter.host_id = None;
            app.workspace.selection = None;
            app.ui_state.selection = None;
            app.last_session_click = None;
            tasks.push(save_ui_state_task(app));
        }
        Message::OpenHostInbox(host_id) => {
            app.inbox_open = true;
            app.notification_filter.host_id = Some(host_id);
            app.workspace.selection = None;
            app.ui_state.selection = None;
            app.last_session_click = None;
            tasks.push(save_ui_state_task(app));
        }
        Message::FilterNotificationStatus(status) => {
            app.inbox_open = true;
            app.notification_filter.status = status;
        }
        Message::FilterNotificationSeverity(severity) => {
            app.inbox_open = true;
            app.notification_filter.severity = severity;
        }
        Message::FilterNotificationKind(kind) => {
            app.inbox_open = true;
            app.notification_filter.kind = kind;
        }
        Message::FilterNotificationProvider(provider) => {
            app.inbox_open = true;
            app.notification_filter.provider = provider;
        }
        Message::FilterNotificationHost(host_id) => {
            app.inbox_open = true;
            app.notification_filter.host_id = host_id;
        }
        Message::ClearNotificationFilters => {
            app.inbox_open = true;
            app.notification_filter = NotificationFilter::default();
        }
        Message::SelectNotification {
            host_id,
            notification_id,
        } => {
            app.inbox_open = true;
            app.workspace
                .select_notification(host_id.clone(), notification_id.clone());
            app.workspace.selection = Some(Selection::Notification {
                host_id: host_id.clone(),
                notification_id: notification_id.clone(),
            });
            app.ui_state.selection = app.workspace.selection.clone();
            app.last_session_click = None;
            tasks.push(save_ui_state_task(app));
        }
        Message::OpenNotificationLink {
            host_id,
            notification_id,
        } => {
            app.workspace.select_notification(host_id, notification_id);
            app.ui_state.selection = app.workspace.selection.clone();
            app.inbox_open = false;
            sync_rename_edit_for_selection(app);
            app.last_session_click = None;
            tasks.push(save_ui_state_task(app));
        }
        Message::ActOnNotification {
            host_id,
            notification_id,
            action,
        } => match notification_action_task(app, host_id, notification_id, action) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::SelectSession {
            host_id,
            session_id,
        } => {
            app.inbox_open = false;
            // A second click on the already-clicked session within the window is
            // a double-click: select as usual, then open it in a terminal.
            let now = Instant::now();
            let double_click = matches!(
                &app.last_session_click,
                Some((last_host, last_session, at))
                    if *last_host == host_id
                        && *last_session == session_id
                        && now.duration_since(*at) <= DOUBLE_CLICK_WINDOW
            );
            app.workspace
                .select_session(host_id.clone(), session_id.clone());
            app.ui_state.selection = Some(Selection::Session {
                host_id: host_id.clone(),
                session_id: session_id.clone(),
            });
            // Seed the rename buffer with the session's current name so the
            // operator edits it rather than starting from blank.
            app.rename_edit = app
                .workspace
                .hosts
                .get(&host_id)
                .and_then(|host| host.sessions.get(&session_id.0))
                .and_then(|session| session.name.clone())
                .unwrap_or_default();
            tasks.push(save_ui_state_task(app));
            if double_click {
                // Reset so a third click starts a fresh pair rather than
                // re-triggering on every subsequent click.
                app.last_session_click = None;
                match attach_task(app, &host_id, &session_id) {
                    Ok(task) => tasks.push(task),
                    Err(err) => app.status = Some(err),
                }
            } else {
                app.last_session_click = Some((host_id, session_id, now));
            }
        }
        Message::SelectProject {
            host_id,
            project_id,
        } => {
            app.inbox_open = false;
            app.workspace
                .select_project(host_id.clone(), project_id.clone());
            app.ui_state.selection = app.workspace.selection.clone();
            app.project_edit.reference = project_id;
            app.selected_action = None;
            app.start.template = None;
            // Preload the project's in-repo provider filters so the picker is
            // populated before the operator opens a provider panel.
            ensure_project_filters_loaded(app);
            // Load the project's actions eagerly so the launch action picker is
            // populated without a manual step.
            if let Ok(task) = list_project_actions_task(app) {
                tasks.push(task);
            }
            tasks.push(save_ui_state_task(app));
        }
        Message::OpenSession {
            host_id,
            session_id,
        } => match attach_task(app, &host_id, &session_id) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::OpenStartModal => {
            app.start = StartForm::default();
            app.template_recipe = None;
            app.prompt_editor = text_editor::Content::new();
            app.modal = ModalView::Start;
        }
        Message::OpenAssistantModal => {
            app.assistant = AssistantForm::default();
            app.assistant_editor = text_editor::Content::new();
            app.modal = ModalView::Assistant;
        }
        Message::CloseModal => app.modal = ModalView::None,
        Message::StartAgentSelected(agent) => app.start.agent = agent,
        Message::StartTemplateSelected(template) => {
            let chosen = (template != BLANK_TEMPLATE_LABEL).then_some(template);
            app.start.template.clone_from(&chosen);
            app.template_recipe = None;
            match chosen {
                Some(action_name) => match resolve_template_task(app, action_name) {
                    Ok(task) => tasks.push(task),
                    Err(err) => app.status = Some(err),
                },
                None => app.prompt_editor = text_editor::Content::new(),
            }
        }
        Message::TemplateResolved(result) => match result {
            Ok(resolved) => {
                app.prompt_editor = text_editor::Content::with_text(&resolved.rendered);
                app.start.agent = AgentChoice::from_wire(&resolved.recipe.agent);
                app.template_recipe = Some(resolved.recipe);
            }
            Err(err) => app.status = Some(err),
        },
        Message::PromptEdited(action) => app.prompt_editor.perform(action),
        Message::AssistantRequestEdited(action) => app.assistant_editor.perform(action),
        Message::AssistantIntentSelected(intent) => app.assistant.intent = intent,
        Message::AssistantAgentSelected(agent) => {
            app.assistant.agent = (agent != ASSISTANT_AUTO_AGENT_LABEL).then_some(agent);
        }
        Message::ToggleAssistantAdvanced => {
            app.assistant.show_advanced = !app.assistant.show_advanced;
        }
        Message::AssistantBranchChanged(value) => app.assistant.branch = value,
        Message::AssistantBaseBranchChanged(value) => app.assistant.base_branch = value,
        Message::AssistantNoSnapshotToggled(value) => app.assistant.no_snapshot = value,
        Message::AssistantDegradedToggled(value) => app.assistant.degraded = value,
        Message::ToggleStartAdvanced => app.start.show_advanced = !app.start.show_advanced,
        Message::StartBranchChanged(value) => app.start.branch = value,
        Message::StartBaseBranchChanged(value) => app.start.base_branch = value,
        Message::StartNameChanged(value) => app.start.name = value,
        Message::CreateSession => match create_session_task(app) {
            Ok(task) => {
                tasks.push(task);
                app.modal = ModalView::None;
            }
            Err(err) => app.status = Some(err),
        },
        Message::LaunchAssistant => match launch_assistant_task(app) {
            Ok(task) => {
                tasks.push(task);
                app.modal = ModalView::None;
            }
            Err(err) => app.status = Some(err),
        },
        Message::RenameEditChanged(value) => app.rename_edit = value,
        Message::RenameSession => match rename_selected_session_task(app, false) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::ClearSessionName => match rename_selected_session_task(app, true) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::OpenLinearIssue(issue_id) => {
            if let Ok(host_id) = selected_host_id(app) {
                app.workspace
                    .apply(CoreMessage::LinearProviderIssueSelected { host_id, issue_id });
                app.modal = ModalView::ProviderItem;
            }
        }
        Message::OpenGitHubPullRequest(number) => {
            if let Ok(host_id) = selected_host_id(app) {
                app.workspace
                    .apply(CoreMessage::GitHubProviderPullRequestSelected { host_id, number });
                app.modal = ModalView::ProviderItem;
            }
        }
        Message::OpenGitHubIssue(number) => {
            if let Ok(host_id) = selected_host_id(app) {
                app.workspace
                    .apply(CoreMessage::GitHubProviderIssueSelected { host_id, number });
                app.modal = ModalView::ProviderItem;
            }
        }
        Message::InspectSelectedSession => match inspect_selected_session_task(app) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::StopSelectedSession => match stop_selected_session_task(app) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::RemoveSelectedSession => match remove_selected_session_task(app) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::MetadataKeyChanged(value) => app.metadata_edit.key = value,
        Message::MetadataValueChanged(value) => app.metadata_edit.value = value,
        Message::SetMetadata => match metadata_task(app, MetadataAction::Set) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::ClearMetadata => match metadata_task(app, MetadataAction::Clear) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::ProjectPathChanged(value) => app.project_edit.path = value,
        Message::ProjectNameChanged(value) => app.project_edit.name = value,
        Message::ProjectBaseBranchChanged(value) => app.project_edit.base_branch = value,
        Message::ProjectRenameToChanged(value) => app.project_edit.rename_to = value,
        Message::AddProject => match add_project_task(app) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::ShowProject => match show_project_task(app) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::RenameProject => match rename_project_task(app) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::CopyWorktreePath(path) => {
            let display = path.display().to_string();
            app.status = Some(format!("Copied path to clipboard: {display}"));
            tasks.push(iced::clipboard::write::<Message>(display));
        }
        Message::RemoveWorktree(path) => match remove_worktree_task(app, path) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::SelectAction(name) => app.selected_action = Some(name),
        Message::SelectProviderPanel(panel) => {
            if let Ok(host_id) = selected_host_id(app) {
                app.workspace
                    .apply(CoreMessage::ProviderPanelSelected { host_id, panel });
                ensure_project_filters_loaded(app);
                // Auto-fetch the panel's data so switching tabs immediately shows
                // results instead of requiring a separate Fetch click.
                match panel {
                    ProviderPanel::Linear => {
                        if let Ok(request_id) = begin_linear_issues_request(app) {
                            push_provider_task_result(
                                app,
                                &mut tasks,
                                SessionLinkProvider::Linear,
                                ProviderOperation::LinearIssues,
                                Some(request_id),
                                fetch_linear_issues_task(app, request_id),
                            );
                        }
                    }
                    ProviderPanel::GitHub => {
                        if let Ok(request_id) = begin_github_pull_requests_request(app) {
                            push_provider_task_result(
                                app,
                                &mut tasks,
                                SessionLinkProvider::GitHub,
                                ProviderOperation::GitHubPullRequests,
                                Some(request_id),
                                fetch_github_pull_requests_task(app, request_id),
                            );
                        }
                        if let Ok(request_id) = begin_github_issues_request(app) {
                            push_provider_task_result(
                                app,
                                &mut tasks,
                                SessionLinkProvider::GitHub,
                                ProviderOperation::GitHubIssues,
                                Some(request_id),
                                fetch_github_issues_task(app, request_id),
                            );
                        }
                    }
                }
            }
        }
        Message::SelectLinearFilter(name) => {
            if let Ok(host_id) = selected_host_id(app) {
                app.workspace
                    .apply(CoreMessage::LinearProviderFilterSelected { host_id, name });
            }
            // Picking a filter both selects it and fetches with it in one click.
            match begin_linear_issues_request(app) {
                Ok(request_id) => {
                    ensure_project_filters_loaded(app);
                    push_provider_task_result(
                        app,
                        &mut tasks,
                        SessionLinkProvider::Linear,
                        ProviderOperation::LinearIssues,
                        Some(request_id),
                        fetch_linear_issues_task(app, request_id),
                    );
                }
                Err(err) => app.status = Some(err),
            }
        }
        Message::SelectGitHubFilter(name) => {
            if let Ok(host_id) = selected_host_id(app) {
                app.workspace
                    .apply(CoreMessage::GitHubProviderFilterSelected { host_id, name });
            }
            // Picking a filter both selects it and fetches PRs in one click.
            match begin_github_pull_requests_request(app) {
                Ok(request_id) => {
                    ensure_project_filters_loaded(app);
                    push_provider_task_result(
                        app,
                        &mut tasks,
                        SessionLinkProvider::GitHub,
                        ProviderOperation::GitHubPullRequests,
                        Some(request_id),
                        fetch_github_pull_requests_task(app, request_id),
                    );
                }
                Err(err) => app.status = Some(err),
            }
        }
        Message::FetchLinearIssues => match begin_linear_issues_request(app) {
            Ok(request_id) => {
                ensure_project_filters_loaded(app);
                push_provider_task_result(
                    app,
                    &mut tasks,
                    SessionLinkProvider::Linear,
                    ProviderOperation::LinearIssues,
                    Some(request_id),
                    fetch_linear_issues_task(app, request_id),
                );
            }
            Err(err) => app.status = Some(err),
        },
        Message::FetchGitHubPullRequests => match begin_github_pull_requests_request(app) {
            Ok(request_id) => {
                ensure_project_filters_loaded(app);
                push_provider_task_result(
                    app,
                    &mut tasks,
                    SessionLinkProvider::GitHub,
                    ProviderOperation::GitHubPullRequests,
                    Some(request_id),
                    fetch_github_pull_requests_task(app, request_id),
                );
            }
            Err(err) => app.status = Some(err),
        },
        Message::FetchGitHubIssues => match begin_github_issues_request(app) {
            Ok(request_id) => push_provider_task_result(
                app,
                &mut tasks,
                SessionLinkProvider::GitHub,
                ProviderOperation::GitHubIssues,
                Some(request_id),
                fetch_github_issues_task(app, request_id),
            ),
            Err(err) => app.status = Some(err),
        },
        Message::FetchGitHubPullRequestStatus => {
            match begin_github_pull_request_status_request(app) {
                Ok(request_id) => push_provider_task_result(
                    app,
                    &mut tasks,
                    SessionLinkProvider::GitHub,
                    ProviderOperation::GitHubPullRequestStatus,
                    Some(request_id),
                    fetch_github_pr_status_task(app, request_id),
                ),
                Err(err) => app.status = Some(err),
            }
        }
        Message::LaunchLinearIssue => {
            push_provider_task_result(
                app,
                &mut tasks,
                SessionLinkProvider::Linear,
                ProviderOperation::Launch,
                None,
                launch_linear_issue_task(app),
            );
            app.modal = ModalView::None;
        }
        Message::LaunchGitHubPullRequest => {
            push_provider_task_result(
                app,
                &mut tasks,
                SessionLinkProvider::GitHub,
                ProviderOperation::Launch,
                None,
                launch_github_pull_request_task(app),
            );
            app.modal = ModalView::None;
        }
        Message::CoreCommandCompleted(result) => match result {
            Ok(message) => {
                // A newly created or explicitly resumed session opens straight
                // into a terminal, the same as double-clicking a live session.
                let opened_session = match &message {
                    CoreMessage::SessionCreated { host_id, session } => {
                        Some((host_id.clone(), session.id.clone()))
                    }
                    CoreMessage::SessionResumed { host_id, result } => {
                        Some((host_id.clone(), result.session.id.clone()))
                    }
                    _ => None,
                };
                // A removed session is gone from the workspace, so clear a
                // selection still pointing at it to avoid a stale detail pane.
                let removed_session = if let CoreMessage::SessionRemoveCompleted {
                    host_id,
                    session_id,
                    result,
                } = &message
                {
                    result
                        .removed
                        .then(|| (host_id.clone(), session_id.clone()))
                } else {
                    None
                };
                let deleted_notification = if let CoreMessage::NotificationDeleteCompleted {
                    host_id,
                    result,
                } = &message
                {
                    result.deleted.then(|| (host_id.clone(), result.id.clone()))
                } else {
                    None
                };
                app.workspace.apply(message);
                if let Some((host_id, session_id)) = removed_session {
                    if app.ui_state.selection
                        == Some(Selection::Session {
                            host_id,
                            session_id,
                        })
                    {
                        app.ui_state.selection = None;
                    }
                }
                if let Some((host_id, notification_id)) = deleted_notification {
                    if app.ui_state.selection
                        == Some(Selection::Notification {
                            host_id,
                            notification_id,
                        })
                    {
                        app.workspace.selection = None;
                        app.ui_state.selection = None;
                        app.inbox_open = true;
                    }
                }
                if let Some((host_id, session_id)) = opened_session {
                    match attach_task(app, &host_id, &session_id) {
                        Ok(task) => tasks.push(task),
                        Err(err) => app.status = Some(err),
                    }
                }
            }
            Err(err) => app.status = Some(err),
        },
        Message::AttachSpawned(result) => {
            app.status = Some(match result {
                Ok(()) => "attach command spawned".to_owned(),
                Err(err) => err,
            });
        }
        Message::NotificationSent(result) | Message::UiStateSaved(result) => {
            if let Err(err) = result {
                app.status = Some(err);
            }
        }
        Message::WindowResized(size) => {
            app.ui_state.window_size = WindowSize {
                width: window_dimension_to_u32(size.width),
                height: window_dimension_to_u32(size.height),
            };
            tasks.push(save_ui_state_task(app));
        }
    }
    Task::batch(tasks)
}

fn push_provider_task_result(
    app: &mut PohunekApp,
    tasks: &mut Vec<Task<Message>>,
    provider: SessionLinkProvider,
    operation: ProviderOperation,
    request_id: Option<ProviderRequestId>,
    result: Result<Task<Message>, String>,
) {
    match result {
        Ok(task) => tasks.push(task),
        Err(error) => match selected_host_id(app) {
            Ok(host_id) => app.workspace.apply(CoreMessage::ProviderOperationFailed {
                host_id,
                provider,
                operation,
                request_id,
                error,
            }),
            Err(_) => app.status = Some(error),
        },
    }
}

fn begin_linear_issues_request(app: &mut PohunekApp) -> Result<ProviderRequestId, String> {
    let host_id = selected_host_id(app)?;
    Ok(app.workspace.begin_linear_issues_request(host_id))
}

fn begin_github_pull_requests_request(app: &mut PohunekApp) -> Result<ProviderRequestId, String> {
    let host_id = selected_host_id(app)?;
    Ok(app.workspace.begin_github_pull_requests_request(host_id))
}

fn begin_github_issues_request(app: &mut PohunekApp) -> Result<ProviderRequestId, String> {
    let host_id = selected_host_id(app)?;
    Ok(app.workspace.begin_github_issues_request(host_id))
}

fn begin_github_pull_request_status_request(
    app: &mut PohunekApp,
) -> Result<ProviderRequestId, String> {
    let host_id = selected_host_id(app)?;
    Ok(app
        .workspace
        .begin_github_pull_request_status_request(host_id))
}

fn discover_hosts_task(config: &AppConfig) -> Task<Message> {
    let local = config.local_host.clone();
    let options = config.connection_options;
    Task::perform(
        runtime::perform(async move {
            match discover_hosts(local.clone(), options).await {
                Ok(hosts) => DiscoveryResult {
                    hosts,
                    warning: None,
                },
                Err(err) => DiscoveryResult {
                    hosts: vec![local],
                    warning: Some(format!("host discovery failed: {err}")),
                },
            }
        }),
        Message::HostsDiscovered,
    )
}

fn notification_tasks(app: &mut PohunekApp) -> Task<Message> {
    let Ok(config) = &app.config else {
        app.notified_intents = app.workspace.notification_intents.len();
        return Task::none();
    };
    let intents = app.workspace.notification_intents[app.notified_intents..].to_vec();
    app.notified_intents = app.workspace.notification_intents.len();
    Task::batch(intents.into_iter().map(|intent| {
        let command = config.notification_command.clone();
        Task::perform(
            async move { spawn_notification(&command, &intent) },
            Message::NotificationSent,
        )
    }))
}

/// Builds and dispatches session creation from the Start modal. The input is the
/// prompt editor's text (typed for a blank session, or the edited template
/// prompt); branch/base come from the resolved template recipe when a template is
/// selected, otherwise from the Advanced overrides. The agent is the picker value.
fn create_session_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let host = selected_host_config(app)?;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    let project = selected_project_reference(app)?;
    let terminal_size = terminal_size(app)?;
    let input = app.prompt_editor.text();
    let (branch, base_branch) = match (&app.start.template, &app.template_recipe) {
        (Some(_), Some(recipe)) => (recipe.branch.clone(), recipe.base_branch.clone()),
        (Some(_), None) => return Err("the selected template is still loading".to_owned()),
        (None, _) => (
            optional_field(&app.start.branch),
            optional_field(&app.start.base_branch),
        ),
    };
    let params = SessionNewParams {
        agent: app.start.agent.as_str().to_owned(),
        name: optional_field(&app.start.name),
        cwd: None,
        cols: terminal_size.cols,
        rows: terminal_size.rows,
        project: Some(project),
        repo: None,
        branch,
        base_branch,
        input: (!input.trim().is_empty()).then_some(input),
        metadata: BTreeMap::new(),
    };
    Ok(Task::perform(
        runtime::perform(async move {
            create_session_with_options(&host, params, options)
                .await
                .map(|result| CoreMessage::SessionCreated {
                    host_id,
                    session: result.session,
                })
                .map_err(|err| err.to_string())
        }),
        Message::CoreCommandCompleted,
    ))
}

fn launch_assistant_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let target = selected_assistant_project(app)?;
    let host_id = target.host.id.clone();
    let options = connection_options(app)?;
    let paths = AssistantPaths::resolve().map_err(|err| err.to_string())?;
    let terminal_size = terminal_size(app)?;
    let request = optional_field(&app.assistant_editor.text());
    let params = AssistantLaunchParams {
        intent: app.assistant.intent,
        request,
        agent: app.assistant.agent.clone(),
        project: Some(target.project_ref),
        repo: None,
        branch: optional_field(&app.assistant.branch),
        base_branch: optional_field(&app.assistant.base_branch),
        cols: terminal_size.cols,
        rows: terminal_size.rows,
        no_snapshot: app.assistant.no_snapshot,
        degraded: app.assistant.degraded,
        auto_started_daemon: false,
    };
    Ok(Task::perform(
        runtime::perform(async move {
            assistant_core::launch_with_options(&target.host, &paths, params, options)
                .await
                .map(|result| CoreMessage::SessionCreated {
                    host_id,
                    session: result.session,
                })
                .map_err(|err| err.to_string())
        }),
        Message::CoreCommandCompleted,
    ))
}

fn inspect_selected_session_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let (host, session_id) = selected_session_target(app)?;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    Ok(Task::perform(
        runtime::perform(async move {
            inspect_session_with_options(&host, &session_id, options)
                .await
                .map(|session| CoreMessage::SessionInspected { host_id, session })
                .map_err(|err| err.to_string())
        }),
        Message::CoreCommandCompleted,
    ))
}

/// Resolves a selected template (a `None`-provider action), renders its static
/// prompt, and returns the rendered text plus recipe so the Start modal can show
/// it in an editable buffer before the operator launches the session.
fn resolve_template_task(app: &PohunekApp, action_name: String) -> Result<Task<Message>, String> {
    let host = selected_host_config(app)?;
    let options = connection_options(app)?;
    let project = selected_project_reference(app)?;
    Ok(Task::perform(
        runtime::perform(async move {
            let action = resolve_project_action_with_options(
                &host,
                ProjectActionParams {
                    reference: project,
                    name: action_name,
                },
                options,
            )
            .await
            .map_err(|err| err.to_string())?;
            let preview = preview_action_prompt(&action, String::new(), String::new())
                .map_err(|err| err.to_string())?;
            Ok(ResolvedTemplate {
                rendered: preview.rendered,
                recipe: TemplateRecipe {
                    agent: action.agent,
                    branch: action.branch,
                    base_branch: action.base_branch,
                },
            })
        }),
        Message::TemplateResolved,
    ))
}

fn stop_selected_session_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let (host, session_id) = selected_session_target(app)?;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    Ok(Task::perform(
        runtime::perform(async move {
            stop_session_with_options(&host, &session_id, options)
                .await
                .map(|result| CoreMessage::SessionStopCompleted {
                    host_id,
                    session_id,
                    result,
                })
                .map_err(|err| err.to_string())
        }),
        Message::CoreCommandCompleted,
    ))
}

fn remove_selected_session_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let (host, session_id) = selected_session_target(app)?;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    Ok(Task::perform(
        runtime::perform(async move {
            remove_session_with_options(&host, &session_id, options)
                .await
                .map(|result| CoreMessage::SessionRemoveCompleted {
                    host_id,
                    session_id,
                    result,
                })
                .map_err(|err| err.to_string())
        }),
        Message::CoreCommandCompleted,
    ))
}

fn notification_action_task(
    app: &PohunekApp,
    host_id: HostId,
    notification_id: NotificationId,
    action: NotificationAction,
) -> Result<Task<Message>, String> {
    let host = host_config(app, &host_id)?;
    let options = connection_options(app)?;
    Ok(match action {
        NotificationAction::Read => notification_update_task(
            host,
            host_id,
            notification_id,
            NotificationStatus::Read,
            options,
        ),
        NotificationAction::Acknowledge => notification_update_task(
            host,
            host_id,
            notification_id,
            NotificationStatus::Acknowledged,
            options,
        ),
        NotificationAction::Archive => notification_update_task(
            host,
            host_id,
            notification_id,
            NotificationStatus::Archived,
            options,
        ),
        NotificationAction::Delete => {
            notification_delete_task(host, host_id, notification_id, options)
        }
    })
}

fn notification_update_task(
    host: HostConfig,
    host_id: HostId,
    notification_id: NotificationId,
    status: NotificationStatus,
    options: ConnectionOptions,
) -> Task<Message> {
    let params = NotificationUpdateParams {
        id: notification_id,
        status,
    };
    Task::perform(
        runtime::perform(async move {
            update_notification_with_options(&host, params, options)
                .await
                .map(|result| CoreMessage::NotificationUpdateCompleted { host_id, result })
                .map_err(|err| err.to_string())
        }),
        Message::CoreCommandCompleted,
    )
}

fn notification_delete_task(
    host: HostConfig,
    host_id: HostId,
    notification_id: NotificationId,
    options: ConnectionOptions,
) -> Task<Message> {
    let params = NotificationDeleteParams {
        id: notification_id,
    };
    Task::perform(
        runtime::perform(async move {
            delete_notification_with_options(&host, params, options)
                .await
                .map(|result| CoreMessage::NotificationDeleteCompleted { host_id, result })
                .map_err(|err| err.to_string())
        }),
        Message::CoreCommandCompleted,
    )
}

fn resume_session_task(
    app: &PohunekApp,
    host_id: &HostId,
    session_id: &SessionId,
) -> Result<Task<Message>, String> {
    let host = host_config(app, host_id)?;
    let host_id = host_id.clone();
    let session_id = session_id.clone();
    let options = connection_options(app)?;
    Ok(Task::perform(
        runtime::perform(async move {
            resume_session_with_options(&host, &session_id, options)
                .await
                .map(|result| CoreMessage::SessionResumed { host_id, result })
                .map_err(|err| err.to_string())
        }),
        Message::CoreCommandCompleted,
    ))
}

#[derive(Debug, Clone, Copy)]
enum MetadataAction {
    Set,
    Clear,
}

fn metadata_task(app: &PohunekApp, action: MetadataAction) -> Result<Task<Message>, String> {
    let (host, session_id) = selected_session_target(app)?;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    let key = required_field(&app.metadata_edit.key, "metadata key")?;
    let value = match action {
        MetadataAction::Set => Some(app.metadata_edit.value.clone()),
        MetadataAction::Clear => None,
    };
    let params = SessionSetMetadataParams {
        session_id,
        metadata: BTreeMap::from([(key, value)]),
    };
    Ok(Task::perform(
        runtime::perform(async move {
            set_session_metadata_with_options(&host, params, options)
                .await
                .map(|result| CoreMessage::SessionMetadataUpdated { host_id, result })
                .map_err(|err| err.to_string())
        }),
        Message::CoreCommandCompleted,
    ))
}

/// Set (`clear == false`) or clear the selected session's display name. The new
/// name is the rename buffer, trimmed daemon-side; clearing ignores the buffer.
fn rename_selected_session_task(app: &PohunekApp, clear: bool) -> Result<Task<Message>, String> {
    let (host, session_id) = selected_session_target(app)?;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    let name = if clear {
        None
    } else {
        optional_field(&app.rename_edit)
    };
    let params = SessionRenameParams { session_id, name };
    Ok(Task::perform(
        runtime::perform(async move {
            rename_session_with_options(&host, params, options)
                .await
                .map(|result| CoreMessage::SessionRenamed { host_id, result })
                .map_err(|err| err.to_string())
        }),
        Message::CoreCommandCompleted,
    ))
}

fn add_project_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let host = selected_host_config(app)?;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    let params = ProjectAddParams {
        path: Some(PathBuf::from(required_field(
            &app.project_edit.path,
            "project path",
        )?)),
        name: optional_field(&app.project_edit.name),
        base_branch: optional_field(&app.project_edit.base_branch),
    };
    Ok(Task::perform(
        runtime::perform(async move {
            add_project_with_options(&host, params, options)
                .await
                .map(|project| CoreMessage::ProjectAdded { host_id, project })
                .map_err(|err| err.to_string())
        }),
        Message::CoreCommandCompleted,
    ))
}

fn show_project_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let host = selected_host_config(app)?;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    let params = ProjectShowParams {
        reference: selected_project_reference(app)?,
    };
    Ok(Task::perform(
        runtime::perform(async move {
            show_project_with_options(&host, params, options)
                .await
                .map(|result| CoreMessage::ProjectShown { host_id, result })
                .map_err(|err| err.to_string())
        }),
        Message::CoreCommandCompleted,
    ))
}

fn rename_project_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let host = selected_host_config(app)?;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    let params = ProjectRenameParams {
        reference: selected_project_reference(app)?,
        name: required_field(&app.project_edit.rename_to, "new project name")?,
    };
    Ok(Task::perform(
        runtime::perform(async move {
            rename_project_with_options(&host, params, options)
                .await
                .map(|project| CoreMessage::ProjectRenamed { host_id, project })
                .map_err(|err| err.to_string())
        }),
        Message::CoreCommandCompleted,
    ))
}

fn remove_worktree_task(app: &PohunekApp, path: PathBuf) -> Result<Task<Message>, String> {
    let host = selected_host_config(app)?;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    let (_host_id, project) =
        selected_project(app).ok_or_else(|| "no project selected".to_owned())?;
    let project_id = project.id.clone();
    let params = WorktreeRemoveParams { path: path.clone() };
    Ok(Task::perform(
        runtime::perform(async move {
            remove_worktree_with_options(&host, params, options)
                .await
                .map(|result| CoreMessage::WorktreeRemoved {
                    host_id,
                    project_id,
                    path,
                    result,
                })
                .map_err(|err| err.to_string())
        }),
        Message::CoreCommandCompleted,
    ))
}

fn list_project_actions_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let host = selected_host_config(app)?;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    let reference = selected_project_reference(app)?;
    let params = ProjectActionsParams {
        reference: reference.clone(),
    };
    Ok(Task::perform(
        runtime::perform(async move {
            match list_project_actions_with_options(&host, params, options).await {
                Ok(result) => Ok(CoreMessage::ProjectActionsLoaded {
                    host_id,
                    reference,
                    result,
                }),
                Err(err) => Ok(CoreMessage::HostOperationFailed {
                    host_id,
                    error: err.to_string(),
                }),
            }
        }),
        Message::CoreCommandCompleted,
    ))
}

fn fetch_linear_issues_task(
    app: &PohunekApp,
    request_id: ProviderRequestId,
) -> Result<Task<Message>, String> {
    let host_id = selected_host_id(app)?;
    let config = app.config.as_ref().map_err(Clone::clone)?;
    let linear = config
        .providers
        .linear
        .clone()
        .ok_or_else(|| "Linear provider is not configured".to_owned())?;
    // The active filter's raw IssueFilter drives the query; the echoed
    // `filter_name` mirrors the host's current selection (possibly `None`) so
    // the stale-result guard rejects results after the picker changes.
    let active_filter = active_linear_filter(app);
    let host = app
        .workspace
        .hosts
        .get(&host_id)
        .ok_or_else(|| format!("unknown host `{host_id}`"))?;
    let filter_name = host.provider.linear.selected_filter.clone();
    let search = host.provider.linear.search.clone();
    let query = providers::linear::LinearQuery {
        filter: active_filter.map(|filter| filter.filter),
        search: optional_field(&search),
        ..providers::linear::LinearQuery::default()
    };
    Ok(Task::perform(
        runtime::perform(async move {
            let client = match providers::linear::HttpGraphqlTransport::try_new() {
                Ok(transport) => providers::linear::LinearClient::new(
                    providers::linear::LinearConfig {
                        token_key: linear.token_key,
                        endpoint: linear.endpoint,
                        token_lookup_timeout: linear.token_lookup_timeout,
                    },
                    providers::linear::KeyringTokenSource::new("pohunek-linear"),
                    transport,
                ),
                Err(err) => {
                    return Ok(CoreMessage::ProviderOperationFailed {
                        host_id,
                        provider: SessionLinkProvider::Linear,
                        operation: ProviderOperation::LinearIssues,
                        request_id: Some(request_id),
                        error: err.to_string(),
                    });
                }
            };
            match client.list_issues(query).await {
                Ok(issues) => Ok(CoreMessage::LinearProviderIssuesLoaded {
                    host_id,
                    request_id,
                    filter_name,
                    search,
                    issues,
                }),
                Err(err) => Ok(CoreMessage::ProviderOperationFailed {
                    host_id,
                    provider: SessionLinkProvider::Linear,
                    operation: ProviderOperation::LinearIssues,
                    request_id: Some(request_id),
                    error: err.to_string(),
                }),
            }
        }),
        Message::CoreCommandCompleted,
    ))
}

fn fetch_github_pull_requests_task(
    app: &PohunekApp,
    request_id: ProviderRequestId,
) -> Result<Task<Message>, String> {
    let (host_id, scope, client) = github_client_for_selected_project(app)?;
    let filter_args = active_github_filter(app)
        .map(|filter| filter.gh_args())
        .unwrap_or_default();
    Ok(Task::perform(
        runtime::perform(async move {
            match client.list_pull_requests(&filter_args).await {
                Ok(pull_requests) => Ok(CoreMessage::GitHubProviderPullRequestsLoaded {
                    host_id,
                    request_id,
                    scope,
                    pull_requests,
                }),
                Err(err) => Ok(CoreMessage::ProviderOperationFailed {
                    host_id,
                    provider: SessionLinkProvider::GitHub,
                    operation: ProviderOperation::GitHubPullRequests,
                    request_id: Some(request_id),
                    error: err.to_string(),
                }),
            }
        }),
        Message::CoreCommandCompleted,
    ))
}

fn fetch_github_issues_task(
    app: &PohunekApp,
    request_id: ProviderRequestId,
) -> Result<Task<Message>, String> {
    let (host_id, scope, client) = github_client_for_selected_project(app)?;
    Ok(Task::perform(
        runtime::perform(async move {
            match client.list_issues().await {
                Ok(issues) => Ok(CoreMessage::GitHubProviderIssuesLoaded {
                    host_id,
                    request_id,
                    scope,
                    issues,
                }),
                Err(err) => Ok(CoreMessage::ProviderOperationFailed {
                    host_id,
                    provider: SessionLinkProvider::GitHub,
                    operation: ProviderOperation::GitHubIssues,
                    request_id: Some(request_id),
                    error: err.to_string(),
                }),
            }
        }),
        Message::CoreCommandCompleted,
    ))
}

fn fetch_github_pr_status_task(
    app: &PohunekApp,
    request_id: ProviderRequestId,
) -> Result<Task<Message>, String> {
    let target = selected_github_pr_status_target(app)?;
    let (host_id, _scope, client) = github_client_for_selected_project(app)?;
    Ok(Task::perform(
        runtime::perform(async move {
            match client.pull_request_status(target.number).await {
                Ok(status) => Ok(CoreMessage::GitHubProviderPullRequestStatusLoaded {
                    host_id,
                    request_id,
                    status_key: target.status_key,
                    status,
                }),
                Err(err) => Ok(CoreMessage::ProviderOperationFailed {
                    host_id,
                    provider: SessionLinkProvider::GitHub,
                    operation: ProviderOperation::GitHubPullRequestStatus,
                    request_id: Some(request_id),
                    error: err.to_string(),
                }),
            }
        }),
        Message::CoreCommandCompleted,
    ))
}

fn launch_linear_issue_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let host = selected_host_config(app)?;
    let host_id = host.id.clone();
    let (project, _) = selected_project_identity(app)?;
    let action_name = launch_action_name(app, &ProviderKind::LinearIssue)?;
    let options = connection_options(app)?;
    let terminal_size = terminal_size(app)?;
    let issue = selected_linear_issue(app)?;
    let context_json = issue.to_prompt_json().to_string();
    let issue_id = issue.prompt_item_id().to_owned();
    let name = optional_field(&app.start.name);
    let item = ProviderLaunchItem::linear_issue(issue_id, context_json, issue.url)
        .map_err(|err| err.to_string())?;
    Ok(Task::perform(
        runtime::perform(async move {
            match launch_provider_item_with_options(
                &host,
                ProviderLaunchParams {
                    project,
                    action_name,
                    item,
                    cols: terminal_size.cols,
                    rows: terminal_size.rows,
                    name,
                },
                options,
            )
            .await
            {
                Ok(result) => Ok(CoreMessage::SessionCreated {
                    host_id,
                    session: result.session,
                }),
                Err(err) => Ok(CoreMessage::HostOperationFailed {
                    host_id,
                    error: err.to_string(),
                }),
            }
        }),
        Message::CoreCommandCompleted,
    ))
}

fn launch_github_pull_request_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let host = selected_host_config(app)?;
    let host_id = host.id.clone();
    let (project, _) = selected_project_identity(app)?;
    let action_name = launch_action_name(app, &ProviderKind::GithubPr)?;
    let options = connection_options(app)?;
    let terminal_size = terminal_size(app)?;
    let pull_request = selected_github_pull_request(app)?;
    let context_json = pull_request.to_prompt_json().to_string();
    let name = optional_field(&app.start.name);
    let item = ProviderLaunchItem::github_pull_request(
        pull_request.prompt_item_id(),
        context_json,
        pull_request.url,
    )
    .map_err(|err| err.to_string())?;
    Ok(Task::perform(
        runtime::perform(async move {
            match launch_provider_item_with_options(
                &host,
                ProviderLaunchParams {
                    project,
                    action_name,
                    item,
                    cols: terminal_size.cols,
                    rows: terminal_size.rows,
                    name,
                },
                options,
            )
            .await
            {
                Ok(result) => Ok(CoreMessage::SessionCreated {
                    host_id,
                    session: result.session,
                }),
                Err(err) => Ok(CoreMessage::HostOperationFailed {
                    host_id,
                    error: err.to_string(),
                }),
            }
        }),
        Message::CoreCommandCompleted,
    ))
}

fn selected_session_target(app: &PohunekApp) -> Result<(HostConfig, SessionId), String> {
    let Some(Selection::Session {
        host_id,
        session_id,
    }) = app.ui_state.selection.clone()
    else {
        return Err("select a session first".to_owned());
    };
    Ok((host_config(app, &host_id)?, session_id))
}

fn sync_rename_edit_for_selection(app: &mut PohunekApp) {
    let Some(Selection::Session {
        host_id,
        session_id,
    }) = app.ui_state.selection.as_ref()
    else {
        return;
    };
    app.rename_edit = app
        .workspace
        .hosts
        .get(host_id)
        .and_then(|host| host.sessions.get(&session_id.0))
        .and_then(|session| session.name.clone())
        .unwrap_or_default();
}

fn selected_host_config(app: &PohunekApp) -> Result<HostConfig, String> {
    let host_id = selected_host_id(app)?;
    host_config(app, &host_id)
}

fn selected_host_id(app: &PohunekApp) -> Result<HostId, String> {
    let host_id = match app.ui_state.selection.as_ref() {
        Some(
            Selection::Host { host_id }
            | Selection::Project { host_id, .. }
            | Selection::Session { host_id, .. }
            | Selection::Notification { host_id, .. },
        ) => Some(host_id.clone()),
        None => app.hosts.first().map(|host| host.id.clone()),
    }
    .ok_or_else(|| "no host is available yet".to_owned())?;
    Ok(host_id)
}

fn host_config(app: &PohunekApp, host_id: &HostId) -> Result<HostConfig, String> {
    app.hosts
        .iter()
        .find(|host| &host.id == host_id)
        .cloned()
        .ok_or_else(|| format!("unknown host `{host_id}`"))
}

fn selected_project_reference(app: &PohunekApp) -> Result<String, String> {
    optional_field(&app.project_edit.reference)
        .or_else(|| match app.ui_state.selection.as_ref() {
            Some(Selection::Project { project_id, .. }) => Some(project_id.clone()),
            Some(Selection::Session {
                host_id,
                session_id,
            }) => app
                .workspace
                .hosts
                .get(host_id)
                .and_then(|host| host.sessions.get(&session_id.0))
                .and_then(|session| session.project_id.clone()),
            _ => None,
        })
        .ok_or_else(|| "select or enter a project reference".to_owned())
}

#[derive(Debug, Clone)]
struct AssistantProjectTarget {
    host: HostConfig,
    project_ref: String,
}

fn selected_assistant_project(app: &PohunekApp) -> Result<AssistantProjectTarget, String> {
    let (host_id, project_ref) = match app.ui_state.selection.as_ref() {
        Some(Selection::Project {
            host_id,
            project_id,
        }) => (host_id.clone(), project_id.clone()),
        Some(Selection::Session {
            host_id,
            session_id,
        }) => {
            let project_ref = app
                .workspace
                .hosts
                .get(host_id)
                .and_then(|host| host.sessions.get(&session_id.0))
                .and_then(|session| session.project_id.clone())
                .ok_or_else(|| "selected session is not linked to a project".to_owned())?;
            (host_id.clone(), project_ref)
        }
        _ => return Err("select a project or project-linked session first".to_owned()),
    };

    Ok(AssistantProjectTarget {
        host: host_config(app, &host_id)?,
        project_ref,
    })
}

fn selected_project_identity(app: &PohunekApp) -> Result<(String, PathBuf), String> {
    match app.ui_state.selection.as_ref() {
        Some(Selection::Project {
            host_id,
            project_id,
        }) => app
            .workspace
            .hosts
            .get(host_id)
            .and_then(|host| host.projects.get(project_id))
            .map(|project| (project.id.clone(), project.repo_root.clone())),
        Some(Selection::Session {
            host_id,
            session_id,
        }) => app.workspace.hosts.get(host_id).and_then(|host| {
            let project_id = host.sessions.get(&session_id.0)?.project_id.as_ref()?;
            host.projects
                .get(project_id)
                .map(|project| (project.id.clone(), project.repo_root.clone()))
        }),
        _ => None,
    }
    .ok_or_else(|| {
        "select a project or linked project session before browsing providers".to_owned()
    })
}

fn selected_github_scope(app: &PohunekApp) -> Result<GitHubProviderScope, String> {
    let (project_id, repo_root) = selected_project_identity(app)?;
    Ok(GitHubProviderScope::new(project_id, repo_root))
}

fn github_client_for_selected_project(
    app: &PohunekApp,
) -> Result<
    (
        HostId,
        GitHubProviderScope,
        providers::github::GitHubClient<providers::github::CommandGhRunner>,
    ),
    String,
> {
    let host_id = selected_host_id(app)?;
    let (project_id, repo_cwd) = selected_project_identity(app)?;
    let scope = GitHubProviderScope::new(project_id, repo_cwd.clone());
    let github = app
        .config
        .as_ref()
        .map_err(Clone::clone)?
        .providers
        .github
        .clone()
        .ok_or_else(|| "GitHub provider is not configured".to_owned())?;
    let config = providers::github::GitHubConfig::new(github.gh_bin)
        .with_repo_cwd(repo_cwd)
        .with_timeout(github.timeout);
    Ok((
        host_id,
        scope,
        providers::github::GitHubClient::with_config(config),
    ))
}

/// In-repo provider filter file, relative to a project's repository root.
const PROJECT_FILTERS_FILE: &str = ".pohunek/providers.toml";

/// Reads a project's in-repo `.pohunek/providers.toml` filter layer, if present.
///
/// Returns `Ok(None)` when the file does not exist (the common case, and what a
/// remote project's locally-unreadable path looks like), so the host and
/// built-in layers stay in effect.
fn load_project_filters(
    repo_root: &std::path::Path,
) -> Result<Option<providers::filters::ProviderFilterSet>, ConfigError> {
    let path = repo_root.join(PROJECT_FILTERS_FILE);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(ConfigError::Read { path, source }),
    };
    let raw: RawProjectFilters =
        toml::from_str(&raw).map_err(|source| ConfigError::Parse { path, source })?;
    Ok(Some(raw.into_filter_set()?))
}

/// Loads the selected project's in-repo filters into the cache once.
///
/// Read or parse failures surface in the status line and leave the host plus
/// built-in layers in effect; a missing file caches an empty layer so it is not
/// re-read on every provider fetch.
fn ensure_project_filters_loaded(app: &mut PohunekApp) {
    let Ok((_, repo_root)) = selected_project_identity(app) else {
        return;
    };
    if app.project_filters.contains_key(&repo_root) {
        return;
    }
    match load_project_filters(&repo_root) {
        Ok(Some(filters)) => {
            app.project_filters.insert(repo_root, filters);
        }
        Ok(None) => {
            app.project_filters
                .insert(repo_root, providers::filters::ProviderFilterSet::default());
        }
        Err(err) => app.status = Some(format!("provider filters: {err}")),
    }
}

/// Resolves the effective provider filters for the current selection.
///
/// Merges the host (`gui.toml`) layer with the selected project's cached in-repo
/// layer, falling back to built-in defaults per provider (see
/// [`providers::filters::merge`]).
fn effective_filters(app: &PohunekApp) -> providers::filters::ProviderFilterSet {
    let host = app
        .config
        .as_ref()
        .map(|config| config.providers.filters.clone())
        .unwrap_or_default();
    let project = selected_project_identity(app)
        .ok()
        .and_then(|(_, repo_root)| app.project_filters.get(&repo_root));
    providers::filters::merge(&host, project)
}

/// Returns the GitHub filter to fetch with: the picked one, else the first
/// effective filter. `None` only when no filters resolve (never, given built-ins).
fn active_github_filter(app: &PohunekApp) -> Option<providers::filters::GitHubFilter> {
    let filters = effective_filters(app);
    let selected = selected_host_id(app)
        .ok()
        .and_then(|host_id| app.workspace.hosts.get(&host_id))
        .and_then(|host| host.provider.github.selected_filter.clone());
    selected
        .and_then(|name| filters.github_filter(&name).cloned())
        .or_else(|| filters.github.first().cloned())
}

/// Returns the Linear filter to fetch with: the picked one, else the first
/// effective filter, paired with its name for the stale-result guard.
fn active_linear_filter(app: &PohunekApp) -> Option<providers::filters::LinearFilter> {
    let filters = effective_filters(app);
    let selected = selected_host_id(app)
        .ok()
        .and_then(|host_id| app.workspace.hosts.get(&host_id))
        .and_then(|host| host.provider.linear.selected_filter.clone());
    selected
        .and_then(|name| filters.linear_filter(&name).cloned())
        .or_else(|| filters.linear.first().cloned())
}

fn selected_linear_issue(app: &PohunekApp) -> Result<providers::linear::LinearIssue, String> {
    let host_id = selected_host_id(app)?;
    let host = app
        .workspace
        .hosts
        .get(&host_id)
        .ok_or_else(|| format!("unknown host `{host_id}`"))?;
    let selected = host
        .provider
        .linear
        .selected_issue_id
        .as_ref()
        .ok_or_else(|| "select a Linear issue first".to_owned())?;
    host.provider
        .linear
        .issues
        .iter()
        .find(|issue| issue.prompt_item_id() == selected)
        .cloned()
        .ok_or_else(|| format!("selected Linear issue `{selected}` is not loaded"))
}

fn selected_github_pull_request(
    app: &PohunekApp,
) -> Result<providers::github::GitHubPullRequest, String> {
    let host_id = selected_host_id(app)?;
    let scope = selected_github_scope(app)?;
    let host = app
        .workspace
        .hosts
        .get(&host_id)
        .ok_or_else(|| format!("unknown host `{host_id}`"))?;
    if host.provider.github.scope.as_ref() != Some(&scope) {
        return Err("fetch GitHub pull requests for the selected project first".to_owned());
    }
    let selected = host
        .provider
        .github
        .selected_pull_request
        .ok_or_else(|| "select a GitHub pull request first".to_owned())?;
    host.provider
        .github
        .pull_requests
        .iter()
        .find(|pull_request| pull_request.number == selected)
        .cloned()
        .ok_or_else(|| format!("selected GitHub pull request `{selected}` is not loaded"))
}

#[derive(Debug, Clone)]
struct GitHubStatusTarget {
    number: u64,
    status_key: GitHubPullRequestStatusKey,
}

fn selected_github_pr_status_target(app: &PohunekApp) -> Result<GitHubStatusTarget, String> {
    let scope = selected_github_scope(app)?;
    if let Some((_, session)) = selected_session(app) {
        if let Some(link) = session_link_metadata(session) {
            if link.provider == SessionLinkProvider::GitHub
                && link.kind == SessionLinkKind::PullRequest
            {
                let number = link
                    .id
                    .parse::<u64>()
                    .map_err(|err| format!("invalid linked GitHub PR id `{}`: {err}", link.id))?;
                return Ok(GitHubStatusTarget {
                    number,
                    status_key: GitHubPullRequestStatusKey::new(scope, link.url),
                });
            }
        }
    }
    let pull_request = selected_github_pull_request(app)?;
    Ok(GitHubStatusTarget {
        number: pull_request.number,
        status_key: GitHubPullRequestStatusKey::new(scope, pull_request.url),
    })
}

/// Action names defined for the selected project whose provider matches
/// `provider`. Empty when actions have not been loaded or none match.
fn available_actions(app: &PohunekApp, provider: &ProviderKind) -> Vec<String> {
    let Ok(host_id) = selected_host_id(app) else {
        return Vec::new();
    };
    let Ok(reference) = selected_project_reference(app) else {
        return Vec::new();
    };
    app.workspace
        .hosts
        .get(&host_id)
        .and_then(|host| host.prompt.actions_by_project.get(&reference))
        .map(|result| {
            result
                .actions
                .iter()
                .filter(|action| action.provider == *provider)
                .map(|action| action.name.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Resolves the launch action name for `provider`: the operator's picked action
/// when valid, otherwise the first matching action defined for the project.
fn launch_action_name(app: &PohunekApp, provider: &ProviderKind) -> Result<String, String> {
    let label = provider.as_str();
    let available = available_actions(app, provider);
    if let Some(selected) = &app.selected_action {
        if available.iter().any(|name| name == selected) {
            return Ok(selected.clone());
        }
    }
    available.into_iter().next().ok_or_else(|| {
        format!("the selected project defines no `{label}` action; add one before launching")
    })
}

fn connection_options(app: &PohunekApp) -> Result<ConnectionOptions, String> {
    app.config
        .as_ref()
        .map(|config| config.connection_options)
        .map_err(Clone::clone)
}

fn terminal_size(app: &PohunekApp) -> Result<TerminalSize, String> {
    app.config
        .as_ref()
        .map(|config| config.terminal_size)
        .map_err(Clone::clone)
}

fn optional_field(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn required_field(value: &str, label: &str) -> Result<String, String> {
    optional_field(value).ok_or_else(|| format!("{label} is required"))
}

fn save_ui_state_task(app: &PohunekApp) -> Task<Message> {
    let Some(state_dir) = app.state_dir.clone() else {
        return Task::none();
    };
    let ui_state = app.ui_state.clone();
    Task::perform(
        async move {
            ui_state
                .save_to_dir(&state_dir)
                .map_err(|err| err.to_string())
        },
        Message::UiStateSaved,
    )
}

fn subscription(app: &PohunekApp) -> Subscription<Message> {
    let mut subscriptions =
        vec![window::resize_events().map(|(_id, size)| Message::WindowResized(size))];
    if let Ok(config) = &app.config {
        subscriptions.extend(app.hosts.iter().cloned().map(|host| {
            Subscription::run_with(
                (host, config.connection_options),
                runtime::host_subscription,
            )
            .map(Message::Core)
        }));
    }
    Subscription::batch(subscriptions)
}

/// Subtle rounded card that groups a detail section so the pane reads as panels
/// rather than a flat stack of text and buttons.
fn card<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(content)
        .padding(16)
        .width(Fill)
        .style(iced::widget::container::rounded_box)
        .into()
}

/// Heading for a detail card.
fn section_title(label: &str) -> Element<'_, Message> {
    text(label).size(18).into()
}

/// Button style for selectable list rows (tree nodes, provider items, monitor
/// rows): flat and transparent, with a hover tint and a filled accent when
/// selected, so lists read as lists rather than a wall of identical buttons.
fn list_row_style(
    selected: bool,
) -> impl Fn(&Theme, iced::widget::button::Status) -> iced::widget::button::Style {
    move |theme, status| {
        use iced::widget::button::{Status, Style};
        let palette = theme.extended_palette();
        let mut style = Style {
            background: None,
            text_color: palette.background.base.text,
            border: iced::border::rounded(6.0),
            ..Style::default()
        };
        if selected {
            style.background = Some(Background::Color(palette.primary.weak.color));
            style.text_color = palette.primary.weak.text;
        } else if matches!(status, Status::Hovered | Status::Pressed) {
            style.background = Some(Background::Color(palette.background.weak.color));
        }
        style
    }
}

/// A full-width selectable list row.
fn list_button<'a>(
    content: impl Into<Element<'a, Message>>,
    message: Message,
    selected: bool,
) -> Element<'a, Message> {
    button(content)
        .width(Fill)
        .padding([6, 10])
        .on_press(message)
        .style(list_row_style(selected))
        .into()
}

/// A flat expand/collapse caret toggle.
fn caret(expanded: bool, node: TreeNodeId) -> Element<'static, Message> {
    button(text(if expanded { "v" } else { ">" }).size(13))
        .padding([2, 6])
        .on_press(Message::ToggleNode(node))
        .style(iced::widget::button::text)
        .into()
}

fn view(app: &PohunekApp) -> Element<'_, Message> {
    let left = column![
        assistant_entry_button(),
        inbox_entry_button(app),
        container(workspace_tree(app))
            .padding(12)
            .height(Fill)
            .style(iced::widget::container::rounded_box),
        container(agents_monitor(app))
            .padding(12)
            .height(u32::from(app.ui_state.agents_pane_height))
            .style(iced::widget::container::rounded_box)
    ]
    .spacing(12);

    let base = container(row![
        container(left).width(u32::from(app.ui_state.left_pane_width)),
        container(detail_view(app)).padding([0, 16]).width(Fill)
    ])
    .padding(16)
    .width(Fill)
    .height(Fill);
    match app.modal {
        ModalView::None => base.into(),
        ModalView::Start => modal(base.into(), start_modal_content(app), Message::CloseModal),
        ModalView::Assistant => modal(
            base.into(),
            assistant_modal_content(app),
            Message::CloseModal,
        ),
        ModalView::ProviderItem => modal(
            base.into(),
            provider_item_modal_content(app),
            Message::CloseModal,
        ),
    }
}

fn inbox_entry_button(app: &PohunekApp) -> Element<'_, Message> {
    let unread = app.workspace.unread_notification_count();
    let label = if unread == 0 {
        "Inbox".to_owned()
    } else {
        format!("Inbox {unread}")
    };
    let button = button(text(label).size(14))
        .width(Fill)
        .padding([8, 10])
        .on_press(Message::OpenInbox);
    if app.inbox_open {
        button.style(iced::widget::button::primary).into()
    } else {
        button.style(iced::widget::button::secondary).into()
    }
}

fn assistant_entry_button() -> Element<'static, Message> {
    button(
        row![text("◎").size(14), text("Assistant").size(14)]
            .spacing(6)
            .align_y(Center),
    )
    .width(Fill)
    .padding([8, 10])
    .on_press(Message::OpenAssistantModal)
    .style(iced::widget::button::primary)
    .into()
}

/// Overlays `dialog` centered on a dimmed backdrop above `base`. Clicking the
/// backdrop sends `on_close`; the dialog itself swallows clicks.
fn modal<'a>(
    base: Element<'a, Message>,
    dialog: Element<'a, Message>,
    on_close: Message,
) -> Element<'a, Message> {
    stack![
        base,
        opaque(
            mouse_area(center(opaque(dialog)).style(|theme: &Theme| {
                iced::widget::container::Style {
                    background: Some(Background::Color(Color {
                        a: 0.8,
                        ..theme.palette().background
                    })),
                    ..iced::widget::container::Style::default()
                }
            }))
            .on_press(on_close)
        )
    ]
    .into()
}

/// A fixed-width rounded dialog body with a title and a close button.
fn dialog_card<'a>(
    title: &'a str,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    let header = row![
        text(title).size(20),
        iced::widget::space().width(Fill),
        button("Close")
            .on_press(Message::CloseModal)
            .style(iced::widget::button::secondary),
    ]
    .align_y(Center);
    container(column![header, content.into()].spacing(16))
        .padding(20)
        .width(640)
        .style(iced::widget::container::rounded_box)
        .into()
}

/// Indents a tree row by depth so the host > project > session hierarchy reads
/// visually without spacer hacks.
fn indent<'a>(depth: u16, content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(content)
        .padding(iced::Padding::ZERO.left(f32::from(depth) * 16.0))
        .into()
}

fn workspace_tree(app: &PohunekApp) -> Element<'_, Message> {
    let mut tree = column![text("Workspace").size(16)].spacing(4);
    if let Err(err) = &app.config {
        tree = tree.push(text(format!("configuration error: {err}")).size(14));
        return scrollable(tree).into();
    }
    for (host_id, host) in &app.workspace.hosts {
        let node = TreeNodeId::host(host_id.clone());
        let expanded = app.ui_state.expanded_nodes.contains(&node);
        let unread = app.workspace.host_unread_notification_count(host_id);
        let mut host_row = row![
            caret(expanded, node),
            conn_dot(host.conn.clone()),
            text(host_id.to_string()).size(15)
        ]
        .spacing(6)
        .align_y(Center);
        if unread > 0 {
            host_row = host_row.push(
                button(text(format!("inbox {unread}")).size(12))
                    .padding([2, 6])
                    .on_press(Message::OpenHostInbox(host_id.clone()))
                    .style(iced::widget::button::text),
            );
        }
        tree = tree.push(host_row);
        if let Some(error) = &host.last_error {
            tree = tree.push(indent(1, text(error).size(12)));
        }
        if expanded {
            tree = push_project_rows(tree, app, host_id, host);
        }
    }
    if app.workspace.hosts.is_empty() {
        tree = tree.push(text("connecting…").size(13));
    }
    scrollable(tree).into()
}

fn push_project_rows<'a>(
    mut tree: iced::widget::Column<'a, Message>,
    app: &'a PohunekApp,
    host_id: &'a HostId,
    host: &'a pohunek_gui_core::HostView,
) -> iced::widget::Column<'a, Message> {
    for project in host.projects.values() {
        tree = push_project_row(tree, app, host_id, host, Some(project));
    }
    let missing_project_ids = host
        .sessions
        .values()
        .filter_map(|session| {
            let project_id = session.project_id.as_ref()?;
            (!host.projects.contains_key(project_id)).then(|| project_id.clone())
        })
        .collect::<BTreeSet<_>>();
    for project_id in missing_project_ids {
        tree = push_missing_project_row(tree, app, host_id, host, &project_id);
    }
    if host
        .sessions
        .values()
        .any(|session| session.project_id.is_none())
    {
        tree = push_project_row(tree, app, host_id, host, None);
    }
    tree
}

fn push_missing_project_row<'a>(
    mut tree: iced::widget::Column<'a, Message>,
    app: &'a PohunekApp,
    host_id: &'a HostId,
    host: &'a pohunek_gui_core::HostView,
    project_id: &str,
) -> iced::widget::Column<'a, Message> {
    let node = TreeNodeId::project(host_id.clone(), project_id);
    let expanded = app.ui_state.expanded_nodes.contains(&node);
    let selected = project_is_selected(app, host_id, project_id);
    tree = tree.push(indent(
        1,
        row![
            caret(expanded, node),
            list_button(
                text(format!("Unknown project {project_id}")).size(14),
                Message::SelectProject {
                    host_id: host_id.clone(),
                    project_id: project_id.to_owned(),
                },
                selected,
            ),
        ]
        .spacing(4)
        .align_y(Center),
    ));
    if expanded {
        for session in host
            .sessions
            .values()
            .filter(|session| session.project_id.as_deref() == Some(project_id))
        {
            tree = tree.push(session_tree_row(app, host_id, host, session));
        }
    }
    tree
}

fn push_project_row<'a>(
    mut tree: iced::widget::Column<'a, Message>,
    app: &'a PohunekApp,
    host_id: &'a HostId,
    host: &'a pohunek_gui_core::HostView,
    project: Option<&'a ProjectInfo>,
) -> iced::widget::Column<'a, Message> {
    let project_id = project.map_or_else(|| "unassigned".to_owned(), |project| project.id.clone());
    let label = project.map_or("No project", |project| project.label.as_str());
    let node = TreeNodeId::project(host_id.clone(), project_id.clone());
    let expanded = app.ui_state.expanded_nodes.contains(&node);
    let selected = project_is_selected(app, host_id, &project_id);
    tree = tree.push(indent(
        1,
        row![
            caret(expanded, node),
            list_button(
                text(label).size(14),
                Message::SelectProject {
                    host_id: host_id.clone(),
                    project_id,
                },
                selected,
            ),
        ]
        .spacing(4)
        .align_y(Center),
    ));
    if expanded {
        for session in host.sessions.values().filter(|session| {
            project.map_or_else(
                || session.project_id.is_none(),
                |project| session.project_id.as_deref() == Some(project.id.as_str()),
            )
        }) {
            tree = tree.push(session_tree_row(app, host_id, host, session));
        }
    }
    tree
}

/// Whether the given project is the current selection (drives row highlight).
fn project_is_selected(app: &PohunekApp, host_id: &HostId, project_id: &str) -> bool {
    matches!(
        app.ui_state.selection.as_ref(),
        Some(Selection::Project { host_id: h, project_id: p }) if h == host_id && p == project_id
    )
}

/// Whether the given session is the current selection (drives row highlight).
fn session_is_selected(app: &PohunekApp, host_id: &HostId, session_id: &SessionId) -> bool {
    matches!(
        app.ui_state.selection.as_ref(),
        Some(Selection::Session { host_id: h, session_id: s }) if h == host_id && s == session_id
    )
}

fn session_tree_row(
    app: &PohunekApp,
    host_id: &HostId,
    host: &pohunek_gui_core::HostView,
    session: &SessionInfo,
) -> Element<'static, Message> {
    let provider_status = linked_pr_status_label(host, session);
    let selected = session_is_selected(app, host_id, &session.id);
    // Lead with the display name when set; otherwise fall back to the id.
    let label = match &session.name {
        Some(name) => format!("{name}  {}{provider_status}", session.agent),
        None => format!("{}  {}{provider_status}", session.id.0, session.agent),
    };
    indent(
        2,
        row![
            status_dot(session.activity),
            list_button(
                text(label).size(14),
                Message::SelectSession {
                    host_id: host_id.clone(),
                    session_id: session.id.clone(),
                },
                selected,
            ),
        ]
        .spacing(6)
        .align_y(Center),
    )
}

/// Append `value` to a middot-separated metadata line, adding the separator only
/// when `line` already has content (so it never starts with a stray separator).
fn push_meta(line: &mut String, value: &str) {
    if !line.is_empty() {
        line.push_str("  ·  ");
    }
    line.push_str(value);
}

fn agents_monitor(app: &PohunekApp) -> Element<'_, Message> {
    let monitor = app.workspace.agent_monitor();
    let filter = app.activity_filter;
    let header = row![
        text("Agents").size(18),
        activity_chip("working", AgentActivity::Working, monitor.working, filter),
        activity_chip("blocked", AgentActivity::Blocked, monitor.blocked, filter),
        activity_chip("idle", AgentActivity::Idle, monitor.idle, filter),
        text(format!("unknown {}", monitor.unknown)).size(13),
    ]
    .spacing(8);
    let mut list = column![header].spacing(5);
    let mut shown = 0_usize;
    for agent in monitor.sessions {
        if filter.is_some() && agent.activity != filter {
            continue;
        }
        shown += 1;
        let selected = session_is_selected(app, &agent.host_id, &agent.session_id);
        // Primary line leads with the display name when set, else the id.
        let primary = match &agent.name {
            Some(name) => format!("{name}  ·  {}", agent.agent),
            None => format!(
                "{} / {}  ·  {}",
                agent.host_id, agent.session_id.0, agent.agent
            ),
        };
        // Secondary line packs the context that was previously missing: host (when
        // a name hid it), project, branch, and the activity word.
        let mut meta = String::new();
        if agent.name.is_some() {
            let _ = write!(&mut meta, "{} / {}", agent.host_id, agent.session_id.0);
        }
        if let Some(project) = agent.project_label.as_ref().or(agent.project_id.as_ref()) {
            push_meta(&mut meta, project);
        }
        if let Some(branch) = &agent.branch {
            push_meta(&mut meta, branch);
        }
        push_meta(
            &mut meta,
            agent.activity.map_or("unknown", AgentActivity::as_str),
        );
        list = list.push(
            row![
                status_dot(agent.activity),
                list_button(
                    column![text(primary).size(14), text(meta).size(11)].spacing(1),
                    Message::SelectSession {
                        host_id: agent.host_id,
                        session_id: agent.session_id,
                    },
                    selected,
                ),
            ]
            .spacing(6)
            .align_y(Center),
        );
    }
    if shown == 0 {
        let empty = if filter.is_some() {
            "No agents match the filter"
        } else {
            "No agents"
        };
        list = list.push(text(empty).size(13));
    }
    scrollable(list).into()
}

/// A clickable activity count chip for the agents monitor. Clicking toggles the
/// monitor's activity filter: selecting an already-active activity clears it.
fn activity_chip(
    label: &str,
    activity: AgentActivity,
    count: usize,
    filter: Option<AgentActivity>,
) -> Element<'static, Message> {
    let active = filter == Some(activity);
    let target = if active { None } else { Some(activity) };
    let content = row![
        status_dot(Some(activity)),
        text(format!("{label} {count}")).size(13)
    ]
    .spacing(4);
    let chip = button(content).on_press(Message::FilterActivity(target));
    if active {
        chip.style(iced::widget::button::primary).into()
    } else {
        chip.style(iced::widget::button::text).into()
    }
}

/// Routes the detail pane to the surface that matches the current selection,
/// instead of stacking every form unconditionally. Sessions show a session
/// card; projects show the project plus its start/provider/action surfaces;
/// hosts show project management; nothing selected shows a start-work landing.
fn detail_view(app: &PohunekApp) -> Element<'_, Message> {
    let body = match app.ui_state.selection.as_ref() {
        Some(Selection::Session { .. }) => session_pane(app),
        Some(Selection::Project { .. }) => project_pane(app),
        Some(Selection::Host { host_id }) => host_pane(app, host_id),
        Some(Selection::Notification { .. }) => notification_pane(app),
        None if app.inbox_open => inbox_pane(app),
        None => start_work_pane(app),
    };
    let mut detail = column![body].spacing(12);
    for toast in app.workspace.toasts.iter().rev().take(3).rev() {
        detail = detail.push(toast_view(toast));
    }
    if let Some(status) = &app.status {
        detail = detail.push(text(status).size(13));
    }
    scrollable(detail).into()
}

/// Landing surface shown when nothing is selected: a guided entry point that
/// lets the operator jump straight into any known project rather than facing an
/// empty form.
fn start_work_pane(app: &PohunekApp) -> Element<'_, Message> {
    let mut projects = column![].spacing(4);
    let mut any_project = false;
    for (host_id, host) in &app.workspace.hosts {
        for project in host.projects.values() {
            any_project = true;
            projects = projects.push(list_button(
                text(format!("{}   ·   {host_id}", project.label)).size(15),
                Message::SelectProject {
                    host_id: host_id.clone(),
                    project_id: project.id.clone(),
                },
                false,
            ));
        }
    }
    if !any_project {
        projects = projects.push(
            text("No projects yet. Select a host in the workspace tree to add one.").size(13),
        );
    }
    column![
        text("Start work").size(22),
        text("Pick a project to start an agent, browse Linear issues, or open a pull request.")
            .size(14),
        card(projects),
    ]
    .spacing(12)
    .into()
}

fn inbox_pane(app: &PohunekApp) -> Element<'_, Message> {
    let unread = app.workspace.unread_notification_count();
    let rows = app.workspace.notifications(&app.notification_filter);
    let header = row![
        text("Inbox").size(22),
        text(format!("{unread} unread")).size(13).style(muted_style),
        iced::widget::space().width(Fill),
        button("Clear filters")
            .on_press(Message::ClearNotificationFilters)
            .style(iced::widget::button::secondary),
    ]
    .spacing(10)
    .align_y(Center);
    let mut list = column![].spacing(5);
    let mut shown = 0_usize;
    for row in rows {
        shown += 1;
        list = list.push(notification_row(app, row.host_id, row.record));
    }
    if shown == 0 {
        list = list.push(text("No notifications match the filters").size(13));
    }
    column![header, notification_filters(app), card(list),]
        .spacing(12)
        .into()
}

fn notification_filters(app: &PohunekApp) -> Element<'_, Message> {
    column![
        notification_status_filters(app),
        notification_severity_filters(app),
        notification_kind_filters(app),
        notification_provider_filters(app),
        notification_host_filters(app),
    ]
    .spacing(4)
    .into()
}

fn notification_status_filters(app: &PohunekApp) -> Element<'static, Message> {
    let statuses = [
        NotificationStatus::Unread,
        NotificationStatus::Read,
        NotificationStatus::Acknowledged,
        NotificationStatus::Archived,
    ];
    let mut row = row![
        text("Status").size(13).style(muted_style),
        notification_chip(
            "all",
            notification_count_with(app, |filter| filter.status = None),
            app.notification_filter.status.is_none(),
            Message::FilterNotificationStatus(None),
        ),
    ]
    .spacing(6)
    .align_y(Center);
    for status in statuses {
        let active = app.notification_filter.status == Some(status);
        row = row.push(notification_chip(
            notification_status_label(status),
            notification_count_with(app, |filter| filter.status = Some(status)),
            active,
            Message::FilterNotificationStatus((!active).then_some(status)),
        ));
    }
    row.into()
}

fn notification_severity_filters(app: &PohunekApp) -> Element<'static, Message> {
    let severities = [
        NotificationSeverity::ActionRequired,
        NotificationSeverity::Error,
        NotificationSeverity::Warning,
        NotificationSeverity::Info,
        NotificationSeverity::Success,
    ];
    let mut row = row![
        text("Severity").size(13).style(muted_style),
        notification_chip(
            "all",
            notification_count_with(app, |filter| filter.severity = None),
            app.notification_filter.severity.is_none(),
            Message::FilterNotificationSeverity(None),
        ),
    ]
    .spacing(6)
    .align_y(Center);
    for severity in severities {
        let active = app.notification_filter.severity == Some(severity);
        row = row.push(notification_chip(
            notification_severity_label(severity),
            notification_count_with(app, |filter| filter.severity = Some(severity)),
            active,
            Message::FilterNotificationSeverity((!active).then_some(severity)),
        ));
    }
    row.into()
}

fn notification_kind_filters(app: &PohunekApp) -> Element<'static, Message> {
    let kinds = [
        NotificationKind::AgentBlocked,
        NotificationKind::ApprovalRequired,
        NotificationKind::Error,
        NotificationKind::TurnCompleted,
        NotificationKind::SessionFinished,
        NotificationKind::System,
    ];
    let mut row = row![
        text("Kind").size(13).style(muted_style),
        notification_chip(
            "all",
            notification_count_with(app, |filter| filter.kind = None),
            app.notification_filter.kind.is_none(),
            Message::FilterNotificationKind(None),
        ),
    ]
    .spacing(6)
    .align_y(Center);
    for kind in kinds {
        let active = app.notification_filter.kind == Some(kind);
        row = row.push(notification_chip(
            notification_kind_label(kind),
            notification_count_with(app, |filter| filter.kind = Some(kind)),
            active,
            Message::FilterNotificationKind((!active).then_some(kind)),
        ));
    }
    row.into()
}

fn notification_provider_filters(app: &PohunekApp) -> Element<'static, Message> {
    let providers = notification_providers(app);
    let mut row = row![
        text("Provider").size(13).style(muted_style),
        notification_chip(
            "all",
            notification_count_with(app, |filter| filter.provider = None),
            app.notification_filter.provider.is_none(),
            Message::FilterNotificationProvider(None),
        ),
    ]
    .spacing(6)
    .align_y(Center);
    for provider in providers {
        let active = app.notification_filter.provider.as_ref() == Some(&provider);
        row = row.push(notification_chip(
            provider.clone(),
            notification_count_with(app, |filter| filter.provider = Some(provider.clone())),
            active,
            Message::FilterNotificationProvider((!active).then_some(provider)),
        ));
    }
    row.into()
}

fn notification_host_filters(app: &PohunekApp) -> Element<'static, Message> {
    let mut row = row![
        text("Host").size(13).style(muted_style),
        notification_chip(
            "all",
            notification_count_with(app, |filter| filter.host_id = None),
            app.notification_filter.host_id.is_none(),
            Message::FilterNotificationHost(None),
        ),
    ]
    .spacing(6)
    .align_y(Center);
    for host_id in app.workspace.hosts.keys() {
        let active = app.notification_filter.host_id.as_ref() == Some(host_id);
        row = row.push(notification_chip(
            host_id.to_string(),
            notification_count_with(app, |filter| filter.host_id = Some(host_id.clone())),
            active,
            Message::FilterNotificationHost((!active).then(|| host_id.clone())),
        ));
    }
    row.into()
}

fn notification_chip(
    label: impl Into<String>,
    count: usize,
    active: bool,
    message: Message,
) -> Element<'static, Message> {
    let chip = button(text(format!("{} {count}", label.into())).size(13)).on_press(message);
    if active {
        chip.style(iced::widget::button::primary).into()
    } else {
        chip.style(iced::widget::button::text).into()
    }
}

fn notification_count_with(
    app: &PohunekApp,
    update: impl FnOnce(&mut NotificationFilter),
) -> usize {
    let mut filter = app.notification_filter.clone();
    update(&mut filter);
    app.workspace.notifications(&filter).len()
}

fn notification_providers(app: &PohunekApp) -> Vec<String> {
    let mut providers = BTreeSet::new();
    for host in app.workspace.hosts.values() {
        for record in host.notifications.values() {
            providers.insert(record.source.provider.clone());
        }
    }
    providers.into_iter().collect()
}

fn notification_row(
    app: &PohunekApp,
    host_id: HostId,
    record: NotificationRecord,
) -> Element<'static, Message> {
    let selected = matches!(
        app.ui_state.selection.as_ref(),
        Some(Selection::Notification { host_id: h, notification_id })
            if h == &host_id && notification_id == &record.id
    );
    let notification_id = record.id.clone();
    let mut meta = String::new();
    push_meta(&mut meta, notification_status_label(record.status));
    push_meta(&mut meta, notification_severity_label(record.severity));
    push_meta(&mut meta, &host_id.to_string());
    push_meta(
        &mut meta,
        record
            .session_id
            .as_ref()
            .map_or("no session", |session_id| session_id.0.as_str()),
    );
    push_meta(&mut meta, notification_kind_label(record.kind));
    push_meta(&mut meta, &notification_age_label(&record.created_at));
    row![
        notification_dot(record.severity),
        list_button(
            column![
                text(record.title)
                    .size(14)
                    .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
                text(meta).size(11).style(muted_style),
            ]
            .spacing(1),
            Message::SelectNotification {
                host_id,
                notification_id,
            },
            selected,
        ),
    ]
    .spacing(6)
    .align_y(Center)
    .into()
}

fn notification_pane(app: &PohunekApp) -> Element<'_, Message> {
    let Some((host_id, record)) = selected_notification(app) else {
        return card(column![
            section_title("Notification"),
            text("Notification not found").size(13)
        ]);
    };
    let mut detail = column![
        section_title("Notification"),
        text(record.title.as_str())
            .size(16)
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
        text(record.body.as_str())
            .size(14)
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
        notification_summary(record, host_id),
        notification_actions(host_id, record),
    ]
    .spacing(8);
    if let Some(link) = notification_link_action(app, host_id, record) {
        detail = detail.push(link);
    }
    detail = detail.push(notification_metadata(record));
    card(detail)
}

fn notification_summary<'a>(
    record: &'a NotificationRecord,
    host_id: &'a HostId,
) -> Element<'a, Message> {
    let mut rows = column![text("Summary").size(15)].spacing(4);
    rows = rows
        .push(text(format!("host: {host_id}")).size(13))
        .push(
            text(format!(
                "status: {}",
                notification_status_label(record.status)
            ))
            .size(13),
        )
        .push(
            text(format!(
                "severity: {}",
                notification_severity_label(record.severity)
            ))
            .size(13),
        )
        .push(text(format!("kind: {}", notification_kind_label(record.kind))).size(13))
        .push(text(format!("created: {}", record.created_at)).size(13))
        .push(
            text(format!(
                "age: {}",
                notification_age_label(&record.created_at)
            ))
            .size(13),
        )
        .push(
            text(format!(
                "source: {} / {} / {}",
                record.source.provider,
                record.source.provider_event,
                record.source.host_local_source_id
            ))
            .size(13),
        );
    if let Some(session_id) = &record.session_id {
        rows = rows.push(text(format!("session: {}", session_id.0)).size(13));
    }
    if let Some(agent_kind) = record.agent_kind {
        rows = rows.push(text(format!("agent: {}", agent_kind_label(agent_kind))).size(13));
    }
    if let Some(project_id) = &record.project_id {
        rows = rows.push(text(format!("project: {project_id}")).size(13));
    }
    rows.into()
}

fn notification_actions(
    host_id: &HostId,
    record: &NotificationRecord,
) -> Element<'static, Message> {
    let mut actions = row![].spacing(8);
    if record.status == NotificationStatus::Unread {
        actions = actions.push(notification_action_button(
            "Mark read",
            host_id,
            &record.id,
            NotificationAction::Read,
            iced::widget::button::secondary,
        ));
    }
    if record.status != NotificationStatus::Acknowledged {
        actions = actions.push(notification_action_button(
            "Acknowledge",
            host_id,
            &record.id,
            NotificationAction::Acknowledge,
            iced::widget::button::secondary,
        ));
    }
    if record.status != NotificationStatus::Archived {
        actions = actions.push(notification_action_button(
            "Archive",
            host_id,
            &record.id,
            NotificationAction::Archive,
            iced::widget::button::secondary,
        ));
    }
    actions = actions.push(notification_action_button(
        "Delete",
        host_id,
        &record.id,
        NotificationAction::Delete,
        iced::widget::button::danger,
    ));
    actions.into()
}

fn notification_action_button(
    label: &'static str,
    host_id: &HostId,
    notification_id: &NotificationId,
    action: NotificationAction,
    style: fn(&Theme, iced::widget::button::Status) -> iced::widget::button::Style,
) -> Element<'static, Message> {
    button(text(label).size(13))
        .padding([5, 9])
        .on_press(Message::ActOnNotification {
            host_id: host_id.clone(),
            notification_id: notification_id.clone(),
            action,
        })
        .style(style)
        .into()
}

fn notification_metadata(record: &NotificationRecord) -> Element<'_, Message> {
    let mut metadata = column![text("Metadata").size(15)].spacing(4);
    if record.metadata.is_empty()
        && record.source_id.is_none()
        && record.dedupe_key.is_none()
        && record.superseded_by.is_none()
    {
        return metadata.push(text("No metadata").size(13)).into();
    }
    for (key, value) in &record.metadata {
        metadata = metadata.push(text(format!("{key}: {value}")).size(13));
    }
    if let Some(source_id) = &record.source_id {
        metadata = metadata.push(text(format!("source_id: {source_id}")).size(13));
    }
    if let Some(dedupe_key) = &record.dedupe_key {
        metadata = metadata.push(text(format!("dedupe_key: {dedupe_key}")).size(13));
    }
    if let Some(superseded_by) = &record.superseded_by {
        metadata = metadata.push(text(format!("superseded_by: {}", superseded_by.0)).size(13));
    }
    metadata.into()
}

fn notification_link_action<'a>(
    app: &'a PohunekApp,
    host_id: &'a HostId,
    record: &'a NotificationRecord,
) -> Option<Element<'a, Message>> {
    let session_id = record.session_id.as_ref()?;
    let live = app
        .workspace
        .hosts
        .get(host_id)
        .is_some_and(|host| host.sessions.contains_key(&session_id.0));
    let content: Element<'a, Message> = if live {
        button("Open linked session")
            .on_press(Message::OpenNotificationLink {
                host_id: host_id.clone(),
                notification_id: record.id.clone(),
            })
            .style(iced::widget::button::primary)
            .into()
    } else {
        text(format!("Linked session {} is no longer live", session_id.0))
            .size(13)
            .style(muted_style)
            .into()
    };
    Some(content)
}

fn selected_notification(app: &PohunekApp) -> Option<(&HostId, &NotificationRecord)> {
    let Some(Selection::Notification {
        host_id,
        notification_id,
    }) = app.ui_state.selection.as_ref()
    else {
        return None;
    };
    app.workspace
        .notification(host_id, notification_id)
        .map(|record| (host_id, record))
}

fn notification_status_label(status: NotificationStatus) -> &'static str {
    match status {
        NotificationStatus::Unread => "unread",
        NotificationStatus::Read => "read",
        NotificationStatus::Acknowledged => "ack",
        NotificationStatus::Archived => "archived",
        NotificationStatus::Deleted => "deleted",
    }
}

fn notification_severity_label(severity: NotificationSeverity) -> &'static str {
    match severity {
        NotificationSeverity::Info => "info",
        NotificationSeverity::Success => "success",
        NotificationSeverity::Warning => "warning",
        NotificationSeverity::Error => "error",
        NotificationSeverity::ActionRequired => "action req",
    }
}

fn notification_kind_label(kind: NotificationKind) -> &'static str {
    match kind {
        NotificationKind::AgentBlocked => "agent blocked",
        NotificationKind::ApprovalRequired => "approval required",
        NotificationKind::TurnCompleted => "turn complete",
        NotificationKind::SessionFinished => "session finished",
        NotificationKind::Error => "error",
        NotificationKind::System => "system",
    }
}

fn agent_kind_label(kind: protocol::AgentKind) -> &'static str {
    match kind {
        protocol::AgentKind::Shell => "shell",
        protocol::AgentKind::Codex => "codex",
        protocol::AgentKind::Claude => "claude",
    }
}

fn notification_dot(severity: NotificationSeverity) -> Element<'static, Message> {
    text(STATUS_DOT)
        .size(13)
        .style(move |theme: &Theme| iced::widget::text::Style {
            color: Some(notification_color(theme, severity)),
        })
        .into()
}

fn notification_color(theme: &Theme, severity: NotificationSeverity) -> iced::Color {
    let palette = theme.extended_palette();
    match severity {
        NotificationSeverity::ActionRequired | NotificationSeverity::Error => {
            palette.danger.base.color
        }
        NotificationSeverity::Warning => palette.warning.base.color,
        NotificationSeverity::Success => palette.success.base.color,
        NotificationSeverity::Info => palette.secondary.base.color,
    }
}

fn notification_age_label(created_at: &str) -> String {
    let Some(created) = parse_rfc3339_utc_seconds(created_at) else {
        return date_part(created_at).to_owned();
    };
    let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
        return date_part(created_at).to_owned();
    };
    let elapsed = now.as_secs().saturating_sub(created);
    if elapsed < SECONDS_PER_MINUTE {
        "now".to_owned()
    } else if elapsed < SECONDS_PER_HOUR {
        format!("{}m", elapsed / SECONDS_PER_MINUTE)
    } else if elapsed < SECONDS_PER_DAY {
        format!("{}h", elapsed / SECONDS_PER_HOUR)
    } else if elapsed < SECONDS_PER_WEEK {
        format!("{}d", elapsed / SECONDS_PER_DAY)
    } else {
        date_part(created_at).to_owned()
    }
}

fn parse_rfc3339_utc_seconds(value: &str) -> Option<u64> {
    let value = value.strip_suffix('Z')?;
    let (date, time) = value.split_once('T')?;
    let mut date_parts = date.split('-');
    let year = date_parts.next()?.parse::<i32>().ok()?;
    let month = date_parts.next()?.parse::<u32>().ok()?;
    let day = date_parts.next()?.parse::<u32>().ok()?;
    if date_parts.next().is_some() || !valid_civil_date(year, month, day) {
        return None;
    }
    let mut time_parts = time.split(':');
    let hour = time_parts.next()?.parse::<u32>().ok()?;
    let minute = time_parts.next()?.parse::<u32>().ok()?;
    let second = time_parts.next()?.split('.').next()?.parse::<u32>().ok()?;
    if time_parts.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days = unix_days_from_civil(year, month, day)?;
    let seconds = days.checked_mul(i64::try_from(SECONDS_PER_DAY).ok()?)?
        + i64::from(hour) * i64::try_from(SECONDS_PER_HOUR).ok()?
        + i64::from(minute) * i64::try_from(SECONDS_PER_MINUTE).ok()?
        + i64::from(second);
    u64::try_from(seconds).ok()
}

fn valid_civil_date(year: i32, month: u32, day: u32) -> bool {
    year >= 1970 && (1..=12).contains(&month) && (1..=days_in_month(year, month)).contains(&day)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn unix_days_from_civil(year: i32, month: u32, day: u32) -> Option<i64> {
    let year = i64::from(year) - i64::from(month <= 2);
    let era = if year >= 0 {
        year
    } else {
        year - (YEARS_PER_ERA - 1)
    } / YEARS_PER_ERA;
    let year_of_era = year - era * YEARS_PER_ERA;
    let month = i64::from(month);
    let month = month
        + if month > 2 {
            -3
        } else {
            MARCH_BASED_MONTH_OFFSET
        };
    let day_of_year = (MONTH_DAY_NUMERATOR * month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    (era * DAYS_PER_ERA + day_of_era).checked_sub(UNIX_EPOCH_DAY_OFFSET)
}

/// Host surface: connection summary plus project management for that host.
fn host_pane<'a>(app: &'a PohunekApp, host_id: &'a HostId) -> Element<'a, Message> {
    let conn = app
        .workspace
        .hosts
        .get(host_id)
        .map_or("unknown", |host| conn_label(&host.conn));
    column![
        text(format!("Host {host_id}")).size(22),
        text(format!("connection: {conn}")).size(14),
        host_projects_view(app, host_id),
    ]
    .spacing(12)
    .into()
}

/// Project surface: project detail plus the start-session, provider-browser and
/// action surfaces, all scoped to this project.
fn project_pane(app: &PohunekApp) -> Element<'_, Message> {
    column![
        project_detail(app),
        button("New session")
            .on_press(Message::OpenStartModal)
            .style(iced::widget::button::primary),
        project_worktrees(app),
        provider_browser_view(app),
    ]
    .spacing(16)
    .into()
}

/// Session surface: the session card with its actions and metadata.
fn session_pane(app: &PohunekApp) -> Element<'_, Message> {
    session_detail(app)
}

fn session_detail(app: &PohunekApp) -> Element<'_, Message> {
    let mut detail = column![section_title("Session")].spacing(8);
    match selected_session(app) {
        Some((host_id, session)) => {
            let activity = session
                .activity
                .map_or("unknown", |activity| activity.as_str());
            // Lead with the display name when set, keeping host/id as a subtitle
            // so the session stays identifiable.
            let heading = match &session.name {
                Some(name) => format!("{name}  ·  {host_id} / {}", session.id.0),
                None => format!("{} / {}", host_id, session.id.0),
            };
            detail = detail
                .push(text(heading).size(16))
                .push(text(format!("agent: {}", session.agent)).size(14))
                .push(text(format!("state: {}", session.state.as_str())).size(14))
                .push(text(format!("activity: {activity}")).size(14));
            if let Some(project) = session
                .project_label
                .as_ref()
                .or(session.project_id.as_ref())
            {
                detail = detail.push(text(format!("project: {project}")).size(14));
            }
            if let Some(branch) = &session.branch {
                detail = detail.push(text(format!("branch: {branch}")).size(14));
            }
            if let Some(link) = session_link_metadata(session) {
                detail = detail.push(text(format!(
                    "linked: {} {} {}",
                    link.provider.as_str(),
                    link.kind.as_str(),
                    link.id
                )));
                if link.provider == SessionLinkProvider::GitHub
                    && link.kind == SessionLinkKind::PullRequest
                {
                    let status = selected_host_config(app)
                        .ok()
                        .and_then(|host| app.workspace.hosts.get(&host.id))
                        .and_then(|host| linked_github_status(host, session));
                    detail = detail.push(text(format!(
                        "PR status: {}",
                        status.unwrap_or_else(|| "unknown".to_owned())
                    )));
                    detail = detail.push(
                        button("Refresh PR status")
                            .on_press(Message::FetchGitHubPullRequestStatus)
                            .style(iced::widget::button::secondary),
                    );
                }
            }
            if let Some(path) = &session.worktree_path {
                detail = detail.push(text(format!("worktree: {}", path.display())).size(14));
            }
            detail = detail.push(text(format!("cwd: {}", session.cwd.display())).size(14));
            detail = detail.push(
                row![
                    button("Open in terminal")
                        .on_press(Message::OpenSession {
                            host_id: host_id.clone(),
                            session_id: session.id.clone(),
                        })
                        .style(iced::widget::button::primary),
                    button("Inspect")
                        .on_press(Message::InspectSelectedSession)
                        .style(iced::widget::button::secondary),
                    button("Stop")
                        .on_press(Message::StopSelectedSession)
                        .style(iced::widget::button::danger),
                    button("Remove")
                        .on_press(Message::RemoveSelectedSession)
                        .style(iced::widget::button::danger)
                ]
                .spacing(8),
            );
            detail = detail.push(rename_view(app));
            detail = detail.push(metadata_view(app, session));
        }
        None => {
            detail = detail.push(text("No session selected").size(16));
        }
    }
    card(detail)
}

/// Rename control for the selected session: a name field plus set/clear buttons,
/// wired to the shared rename buffer. Clearing reverts the session to id-only
/// display.
fn rename_view(app: &PohunekApp) -> Element<'_, Message> {
    column![
        text("Rename").size(16),
        row![
            text_input("new session name", &app.rename_edit)
                .on_input(Message::RenameEditChanged)
                .on_submit(Message::RenameSession),
            button("Rename")
                .on_press(Message::RenameSession)
                .style(iced::widget::button::secondary),
            button("Clear name")
                .on_press(Message::ClearSessionName)
                .style(iced::widget::button::secondary),
        ]
        .spacing(8),
    ]
    .spacing(6)
    .into()
}

fn metadata_view<'a>(app: &'a PohunekApp, session: &'a SessionInfo) -> Element<'a, Message> {
    let mut metadata = column![text("Metadata").size(16)].spacing(6);
    let rows = session_metadata_rows(session);
    if rows.is_empty() {
        metadata = metadata.push(text("No metadata").size(13));
    } else {
        for row in rows {
            metadata = metadata.push(text(format!("{} = {}", row.key, row.value)).size(13));
        }
    }
    metadata = metadata
        .push(
            row![
                text_input("key", &app.metadata_edit.key).on_input(Message::MetadataKeyChanged),
                text_input("value", &app.metadata_edit.value)
                    .on_input(Message::MetadataValueChanged)
            ]
            .spacing(8),
        )
        .push(
            row![
                button("Set metadata")
                    .on_press(Message::SetMetadata)
                    .style(iced::widget::button::secondary),
                button("Clear key")
                    .on_press(Message::ClearMetadata)
                    .style(iced::widget::button::secondary)
            ]
            .spacing(8),
        );
    metadata.into()
}

fn project_detail(app: &PohunekApp) -> Element<'_, Message> {
    let mut detail = column![section_title("Project")].spacing(8);
    if let Some((host_id, project)) = selected_project(app) {
        detail = detail
            .push(text(format!("{} / {}", host_id, project.id)).size(16))
            .push(text(format!("label: {}", project.label)).size(14))
            .push(text(format!("repo: {}", project.repo_root.display())).size(14))
            .push(text(format!("source: {}", project.source.as_str())).size(14));
    } else {
        detail = detail.push(text("No project selected").size(16));
    }
    let detail = detail.push(text("Rename").size(15)).push(
        row![
            text_input("new name", &app.project_edit.rename_to)
                .on_input(Message::ProjectRenameToChanged),
            button("Rename")
                .on_press(Message::RenameProject)
                .style(iced::widget::button::secondary),
        ]
        .spacing(8),
    );
    card(detail)
}

/// Worktree surface for the selected project: a scannable list of every git
/// worktree (live session first, then pohunek-owned, then external) with a
/// status dot, branch, ownership and per-row actions, instead of a flat wall of
/// `path branch=… session=…` lines. Per-worktree removal is intentionally absent
/// — the protocol exposes pruning only via project-level Remove + prune.
fn project_worktrees(app: &PohunekApp) -> Element<'_, Message> {
    let refresh = button("Refresh")
        .on_press(Message::ShowProject)
        .style(iced::widget::button::secondary);
    let header = row![
        section_title("Worktrees"),
        iced::widget::space().width(Fill),
        refresh,
    ]
    .align_y(Center);

    let Some((host_id, project)) = selected_project(app) else {
        return card(column![header, text("No project selected").size(13)].spacing(10));
    };
    let Some(host) = app.workspace.hosts.get(host_id) else {
        return card(column![header, text("Host is not loaded").size(13)].spacing(10));
    };
    let Some(details) = host.project_details.get(&project.id) else {
        return card(
            column![
                header,
                text("Worktree details not loaded yet — Refresh to list them.").size(13),
            ]
            .spacing(10),
        );
    };

    if details.worktrees.is_empty() {
        return card(column![header, text("No worktrees for this project.").size(13)].spacing(10));
    }

    // Live sessions first, then pohunek-owned, then external; stable by path
    // within each group so the list does not jump around between refreshes.
    let mut worktrees: Vec<&ProjectWorktree> = details.worktrees.iter().collect();
    worktrees.sort_by(|a, b| {
        b.session_id
            .is_some()
            .cmp(&a.session_id.is_some())
            .then_with(|| b.owned.cmp(&a.owned))
            .then_with(|| a.path.cmp(&b.path))
    });
    let total = worktrees.len();
    let owned = worktrees.iter().filter(|worktree| worktree.owned).count();
    let active = worktrees
        .iter()
        .filter(|worktree| worktree.session_id.is_some())
        .count();

    let mut list = column![].spacing(6);
    for worktree in worktrees {
        list = list.push(worktree_row(host_id, host, worktree));
    }

    card(
        column![
            header,
            text(format!(
                "{total} worktrees · {owned} owned · {active} active"
            ))
            .size(13),
            scrollable(list).height(360),
        ]
        .spacing(10),
    )
}

/// One worktree row: status dot, basename + meta subtitle, and right-aligned
/// actions (always Copy path; "Open session" when a live session runs in it,
/// which navigates the detail pane to that session).
fn worktree_row<'a>(
    host_id: &'a HostId,
    host: &'a pohunek_gui_core::HostView,
    worktree: &'a ProjectWorktree,
) -> Element<'a, Message> {
    // The branch is the meaningful identifier — basenames collide (most
    // worktrees are named after the repo, e.g. "connection"), so it leads the
    // row; the absolute path and ownership are the wrapping detail line.
    let branch = worktree.branch.as_deref().unwrap_or("detached");
    let owner = if worktree.owned { "owned" } else { "external" };
    let mut meta = format!("{}  ·  {owner}", worktree.path.display());
    if worktree.locked {
        meta.push_str("  ·  locked");
    }
    // `width(Fill)` lets the info column take the remaining width and wrap the
    // long path, so the actions stay inside the card instead of being pushed off
    // the right edge.
    // Paths and branches have no spaces, so default word wrapping cannot break
    // them; `WordOrGlyph` falls back to glyph wrapping so a long path folds
    // inside the column instead of overflowing the card.
    let info = column![
        text(branch)
            .size(14)
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
        text(meta)
            .size(12)
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
            .style(|theme: &Theme| {
                iced::widget::text::Style {
                    color: Some(theme.extended_palette().background.strong.color),
                }
            }),
    ]
    .spacing(2)
    .width(Fill);

    let mut actions = row![button(text("Copy path").size(12))
        .padding([4, 8])
        .on_press(Message::CopyWorktreePath(worktree.path.clone()))
        .style(iced::widget::button::secondary)]
    .spacing(6);
    // Only offer navigation when the session is actually live on this host, so
    // the target session pane has something to show.
    if let Some(session_id) = worktree
        .session_id
        .as_ref()
        .filter(|session_id| host.sessions.contains_key(session_id.as_str()))
    {
        actions = actions.push(
            button(text("Open").size(12))
                .padding([4, 8])
                .on_press(Message::OpenSession {
                    host_id: host_id.clone(),
                    session_id: SessionId(session_id.clone()),
                })
                .style(iced::widget::button::primary),
        );
    }
    // Remove only a pohunek-owned worktree with no live session: the daemon
    // refuses an external worktree (`worktree_not_owned`) and one a live session
    // uses (`worktree_in_use`), so do not offer the button in those cases.
    if worktree.owned && worktree.session_id.is_none() {
        actions = actions.push(
            button(text("Remove").size(12))
                .padding([4, 8])
                .on_press(Message::RemoveWorktree(worktree.path.clone()))
                .style(iced::widget::button::danger),
        );
    }

    let row = row![
        worktree_dot(worktree.owned, worktree.session_id.is_some()),
        info,
        actions,
    ]
    .spacing(10)
    .align_y(Center);

    container(row)
        .padding([8, 10])
        .width(Fill)
        .style(|theme: &Theme| iced::widget::container::Style {
            background: Some(Background::Color(
                theme.extended_palette().background.weak.color,
            )),
            border: iced::border::rounded(6.0),
            ..iced::widget::container::Style::default()
        })
        .into()
}

/// Filled-circle indicator for a worktree: success (green) when a session is
/// live in it, accent when pohunek owns it but is idle, muted for an external
/// worktree pohunek did not create.
fn worktree_dot(owned: bool, active: bool) -> Element<'static, Message> {
    text(STATUS_DOT)
        .size(13)
        .style(move |theme: &Theme| {
            let palette = theme.extended_palette();
            let color = if active {
                palette.success.base.color
            } else if owned {
                palette.primary.base.color
            } else {
                palette.background.strong.color
            };
            iced::widget::text::Style { color: Some(color) }
        })
        .into()
}

/// "Start a session" modal. The operator picks the agent and an optional
/// template; the prompt editor holds the session input (typed for a blank
/// session, or the editable rendered template). Branch/base overrides for a
/// blank session hide behind Advanced; a template supplies its own.
fn start_modal_content(app: &PohunekApp) -> Element<'_, Message> {
    let advanced_label = if app.start.show_advanced {
        "Advanced v"
    } else {
        "Advanced >"
    };
    let mut template_options = vec![BLANK_TEMPLATE_LABEL.to_owned()];
    template_options.extend(available_actions(app, &ProviderKind::None));
    let template_selected = Some(
        app.start
            .template
            .clone()
            .unwrap_or_else(|| BLANK_TEMPLATE_LABEL.to_owned()),
    );
    let prompt_label = if app.start.template.is_some() {
        "Prompt (edit before starting)"
    } else {
        "Prompt / initial input (optional)"
    };
    let mut panel = column![
        row![
            text("Agent").size(14),
            pick_list(
                AgentChoice::ALL,
                Some(app.start.agent),
                Message::StartAgentSelected
            ),
            text("Template").size(14),
            pick_list(
                template_options,
                template_selected,
                Message::StartTemplateSelected
            ),
        ]
        .spacing(8)
        .align_y(Center),
        session_name_input(app),
        text(prompt_label).size(13),
        text_editor(&app.prompt_editor)
            .height(220)
            .on_action(Message::PromptEdited),
        button(text(advanced_label).size(13))
            .on_press(Message::ToggleStartAdvanced)
            .style(iced::widget::button::text),
    ]
    .spacing(8);
    if app.start.show_advanced && app.start.template.is_none() {
        panel = panel.push(
            row![
                text_input("branch override", &app.start.branch)
                    .on_input(Message::StartBranchChanged),
                text_input("base branch override", &app.start.base_branch)
                    .on_input(Message::StartBaseBranchChanged),
            ]
            .spacing(8),
        );
    }
    let panel = panel.push(
        button("Start session")
            .on_press(Message::CreateSession)
            .style(iced::widget::button::primary),
    );
    dialog_card("Start a session", panel)
}

fn assistant_modal_content(app: &PohunekApp) -> Element<'_, Message> {
    let advanced_label = if app.assistant.show_advanced {
        "Advanced v"
    } else {
        "Advanced >"
    };
    let context = selected_assistant_project(app).map_or_else(std::convert::identity, |target| {
        format!("{}  ·  {}", target.host.id, target.project_ref)
    });
    let agent_options = assistant_agent_options(app);
    let selected_agent = Some(
        app.assistant
            .agent
            .clone()
            .unwrap_or_else(|| ASSISTANT_AUTO_AGENT_LABEL.to_owned()),
    );
    let mut panel = column![
        text(context).size(13),
        row![
            text("Intent").size(14),
            pick_list(
                [
                    AssistantIntent::Help,
                    AssistantIntent::Setup,
                    AssistantIntent::Project,
                    AssistantIntent::Update,
                    AssistantIntent::Debug,
                ],
                Some(app.assistant.intent),
                Message::AssistantIntentSelected,
            ),
            text("Agent").size(14),
            pick_list(
                agent_options,
                selected_agent,
                Message::AssistantAgentSelected,
            ),
        ]
        .spacing(8)
        .align_y(Center),
        text("Request / initial prompt").size(13),
        text_editor(&app.assistant_editor)
            .height(180)
            .on_action(Message::AssistantRequestEdited),
        button(text(advanced_label).size(13))
            .on_press(Message::ToggleAssistantAdvanced)
            .style(iced::widget::button::text),
    ]
    .spacing(8);
    if app.assistant.show_advanced {
        panel = panel
            .push(
                row![
                    text_input("branch override", &app.assistant.branch)
                        .on_input(Message::AssistantBranchChanged),
                    text_input("base branch override", &app.assistant.base_branch)
                        .on_input(Message::AssistantBaseBranchChanged),
                ]
                .spacing(8),
            )
            .push(
                row![
                    checkbox(app.assistant.no_snapshot)
                        .label("No snapshot")
                        .on_toggle(Message::AssistantNoSnapshotToggled),
                    checkbox(app.assistant.degraded)
                        .label("Degraded")
                        .on_toggle(Message::AssistantDegradedToggled),
                ]
                .spacing(12),
            );
    }
    let panel = panel.push(
        button("Start assistant")
            .on_press(Message::LaunchAssistant)
            .style(iced::widget::button::primary),
    );
    dialog_card("Start assistant", panel)
}

fn assistant_agent_options(app: &PohunekApp) -> Vec<String> {
    let mut options = vec![
        ASSISTANT_AUTO_AGENT_LABEL.to_owned(),
        "pohunek-assistant".to_owned(),
        "codex".to_owned(),
        "claude".to_owned(),
    ];
    if let Ok(host_id) = selected_host_id(app) {
        if let Some(host) = app.workspace.hosts.get(&host_id) {
            for session in host.sessions.values() {
                if session.agent != "shell"
                    && !options.iter().any(|option| option == &session.agent)
                {
                    options.push(session.agent.clone());
                }
            }
        }
    }
    options
}

/// A labeled "Name" text input bound to the shared start-form name buffer, used
/// by every session-creation surface so a session can be named at any creation.
fn session_name_input(app: &PohunekApp) -> Element<'_, Message> {
    row![
        text("Name").size(14),
        text_input("optional session name", &app.start.name).on_input(Message::StartNameChanged),
    ]
    .spacing(8)
    .align_y(Center)
    .into()
}

/// Modal showing the selected provider item's detail and its launch action.
fn provider_item_modal_content(app: &PohunekApp) -> Element<'_, Message> {
    let host_id = match selected_host_id(app) {
        Ok(host_id) => host_id,
        Err(err) => return dialog_card("Provider item", text(err).size(13)),
    };
    let Some(host) = app.workspace.hosts.get(&host_id) else {
        return dialog_card("Provider item", text("Host is not loaded").size(13));
    };
    let selected_action = app.selected_action.clone();
    match host.provider.active_panel {
        ProviderPanel::Linear => {
            let Some(issue) = selected_linear_issue_in_state(&host.provider.linear) else {
                return dialog_card("Linear issue", text("No issue selected").size(13));
            };
            let body = column![
                text(format!("{}  {}", issue.prompt_item_id(), issue.title)).size(16),
                text(issue.url.clone()).size(13),
                scrollable(text(issue.body.clone()).size(13)).height(260),
                session_name_input(app),
                action_launcher(
                    available_actions(app, &ProviderKind::LinearIssue),
                    selected_action,
                    Message::LaunchLinearIssue,
                ),
            ]
            .spacing(10);
            dialog_card("Linear issue", body)
        }
        ProviderPanel::GitHub => {
            if let Some(pull_request) = selected_pull_request_in_state(&host.provider.github) {
                let body = column![
                    text(format!("#{}  {}", pull_request.number, pull_request.title)).size(16),
                    text(format!(
                        "{}  {}",
                        pull_request.head_ref_name, pull_request.url
                    ))
                    .size(13),
                    scrollable(text(pull_request.body.clone()).size(13)).height(260),
                    session_name_input(app),
                    action_launcher(
                        available_actions(app, &ProviderKind::GithubPr),
                        selected_action,
                        Message::LaunchGitHubPullRequest,
                    ),
                ]
                .spacing(10);
                return dialog_card("Pull request", body);
            }
            if let Some(issue) = selected_github_issue_in_state(&host.provider.github) {
                let body = column![
                    text(format!("#{}  {}", issue.number, issue.title)).size(16),
                    text(issue.url.clone()).size(13),
                    scrollable(text(issue.body.clone()).size(13)).height(260),
                    text("GitHub issues are reference-only; launch from a pull request.").size(12),
                ]
                .spacing(10);
                return dialog_card("GitHub issue", body);
            }
            dialog_card("GitHub", text("No item selected").size(13))
        }
    }
}

/// Host-scoped project surface: the host's registered projects (each selectable)
/// plus an "Add project" form. Rename/remove live in the project surface, scoped
/// to the selected project, instead of a generic reference field here.
fn host_projects_view<'a>(app: &'a PohunekApp, host_id: &'a HostId) -> Element<'a, Message> {
    let mut view = column![section_title("Projects")].spacing(8);
    match app.workspace.hosts.get(host_id) {
        Some(host) if !host.projects.is_empty() => {
            for project in host.projects.values() {
                view = view.push(list_button(
                    text(format!("{}   ({})", project.label, project.id)).size(14),
                    Message::SelectProject {
                        host_id: host_id.clone(),
                        project_id: project.id.clone(),
                    },
                    project_is_selected(app, host_id, &project.id),
                ));
            }
        }
        _ => view = view.push(text("No projects registered on this host").size(13)),
    }
    let view = view.push(text("Add project").size(15)).push(
        row![
            text_input("path", &app.project_edit.path).on_input(Message::ProjectPathChanged),
            text_input("name (optional)", &app.project_edit.name)
                .on_input(Message::ProjectNameChanged),
            text_input("base branch (optional)", &app.project_edit.base_branch)
                .on_input(Message::ProjectBaseBranchChanged),
            button("Add")
                .on_press(Message::AddProject)
                .style(iced::widget::button::secondary),
        ]
        .spacing(8),
    );
    card(view)
}

fn provider_browser_view(app: &PohunekApp) -> Element<'_, Message> {
    let host_id = match selected_host_id(app) {
        Ok(host_id) => host_id,
        Err(err) => return column![text("Providers").size(18), text(err).size(13)].into(),
    };
    let Some(host) = app.workspace.hosts.get(&host_id) else {
        return column![
            text("Providers").size(18),
            text("Host is not loaded").size(13)
        ]
        .into();
    };
    let active = host.provider.active_panel;
    let tab_style = |panel: ProviderPanel| {
        if panel == active {
            iced::widget::button::primary
        } else {
            iced::widget::button::secondary
        }
    };
    let tabs = row![
        button("Linear")
            .on_press(Message::SelectProviderPanel(ProviderPanel::Linear))
            .style(tab_style(ProviderPanel::Linear)),
        button("GitHub")
            .on_press(Message::SelectProviderPanel(ProviderPanel::GitHub))
            .style(tab_style(ProviderPanel::GitHub))
    ]
    .spacing(8);
    let current_scope = selected_github_scope(app).ok();
    let filters = effective_filters(app);
    let body = match active {
        ProviderPanel::Linear => {
            linear_provider_view(host_id.clone(), host, filters.linear_names())
        }
        ProviderPanel::GitHub => {
            github_provider_view(host_id.clone(), current_scope, host, filters.github_names())
        }
    };
    card(column![section_title("Providers"), tabs, body].spacing(10))
}

/// Renders the action picker and launch button for a selected provider item.
/// When the project defines no matching action, shows guidance rather than a
/// launch button that would fail.
fn action_launcher(
    actions: Vec<String>,
    selected_action: Option<String>,
    launch: Message,
) -> Element<'static, Message> {
    if actions.is_empty() {
        return text("No matching action defined for this project; add one to launch")
            .size(13)
            .into();
    }
    let selected = selected_action
        .filter(|name| actions.contains(name))
        .or_else(|| actions.first().cloned());
    row![
        text("Action").size(13),
        pick_list(actions, selected, Message::SelectAction),
        button("Launch")
            .on_press(launch)
            .style(iced::widget::button::primary),
    ]
    .spacing(8)
    .align_y(Center)
    .into()
}

/// Renders one selectable button per named filter; the active filter is styled
/// as primary. The picked name (or the first, when none is picked) highlights.
fn filter_buttons(
    filter_names: Vec<String>,
    selected: Option<&str>,
    make_message: impl Fn(String) -> Message,
) -> Element<'static, Message> {
    let active = selected
        .filter(|name| filter_names.iter().any(|candidate| candidate == name))
        .map(ToOwned::to_owned)
        .or_else(|| filter_names.first().cloned());
    let mut row = iced::widget::Row::new().spacing(6);
    for name in filter_names {
        let is_active = active.as_deref() == Some(name.as_str());
        let style = if is_active {
            iced::widget::button::primary
        } else {
            iced::widget::button::secondary
        };
        let message = make_message(name.clone());
        row = row.push(button(text(name).size(13)).on_press(message).style(style));
    }
    row.into()
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "owning host_id keeps the returned Iced element lifetime tied only to host state"
)]
fn linear_provider_view(
    host_id: HostId,
    host: &pohunek_gui_core::HostView,
    filter_names: Vec<String>,
) -> Element<'_, Message> {
    let state = &host.provider.linear;
    let filters = filter_buttons(
        filter_names,
        state.selected_filter.as_deref(),
        Message::SelectLinearFilter,
    );
    let mut view = column![
        filters,
        row![
            text_input("search", &state.search).on_input({
                let host_id = host_id.clone();
                move |value| {
                    Message::Core(CoreMessage::LinearProviderSearchChanged {
                        host_id: host_id.clone(),
                        value,
                    })
                }
            }),
            button("Fetch")
                .on_press(Message::FetchLinearIssues)
                .style(iced::widget::button::secondary),
        ]
        .spacing(8)
    ]
    .spacing(8);
    if !state.issues.is_empty() {
        view = view.push(text("Pick an issue, choose an action, then Launch.").size(12));
    }
    for issue in &state.issues {
        let issue_id = issue.prompt_item_id().to_owned();
        view = view.push(list_button(
            text(format!("{}  {}", issue.prompt_item_id(), issue.title)).size(13),
            Message::OpenLinearIssue(issue_id),
            false,
        ));
    }
    if let Some(error) = &state.last_error {
        view = view.push(text(error).size(13));
    }
    view.into()
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "owning host_id keeps the returned Iced element lifetime tied only to host state"
)]
fn github_provider_view(
    host_id: HostId,
    current_scope: Option<GitHubProviderScope>,
    host: &pohunek_gui_core::HostView,
    filter_names: Vec<String>,
) -> Element<'_, Message> {
    let state = &host.provider.github;
    // The PR filter (gh search) drives `Fetch PRs`; `search` below is a local
    // text filter applied to the already-fetched rows.
    let filters = filter_buttons(
        filter_names,
        state.selected_filter.as_deref(),
        Message::SelectGitHubFilter,
    );
    let pr_filter_row = row![
        filters,
        button("Fetch PRs")
            .on_press(Message::FetchGitHubPullRequests)
            .style(iced::widget::button::secondary),
    ]
    .spacing(8);
    let mut view = column![
        pr_filter_row,
        row![
            text_input("search", &state.search).on_input({
                let host_id = host_id.clone();
                move |value| {
                    Message::Core(CoreMessage::GitHubProviderSearchChanged {
                        host_id: host_id.clone(),
                        value,
                    })
                }
            }),
            button("Fetch issues")
                .on_press(Message::FetchGitHubIssues)
                .style(iced::widget::button::secondary),
            button("Refresh PR status")
                .on_press(Message::FetchGitHubPullRequestStatus)
                .style(iced::widget::button::secondary),
        ]
        .spacing(8)
    ]
    .spacing(8);
    if state.scope != current_scope {
        if state.scope.is_some() {
            view = view.push(text("Fetch GitHub data for the selected project").size(13));
        }
        if let Some(error) = &state.last_error {
            view = view.push(text(error).size(13));
        }
        return view.into();
    }
    view = view.push(text("Open a pull request to launch a session.").size(12));
    view = view.push(text("Pull requests").size(15));
    for pull_request in filtered_pull_requests(state) {
        let selected = state.selected_pull_request == Some(pull_request.number);
        view = view.push(pull_request_row(pull_request, selected));
    }
    view = view.push(text("Issues").size(15));
    for issue in filtered_github_issues(state) {
        view = view.push(list_button(
            text(format!("#{}  {}", issue.number, issue.title)).size(13),
            Message::OpenGitHubIssue(issue.number),
            false,
        ));
    }
    if let Some(error) = &state.last_error {
        view = view.push(text(error).size(13));
    }
    view.into()
}

fn selected_linear_issue_in_state(
    state: &pohunek_gui_core::LinearProviderState,
) -> Option<&providers::linear::LinearIssue> {
    let selected = state.selected_issue_id.as_ref()?;
    state
        .issues
        .iter()
        .find(|issue| issue.prompt_item_id() == selected)
}

fn selected_pull_request_in_state(
    state: &pohunek_gui_core::GitHubProviderState,
) -> Option<&providers::github::GitHubPullRequest> {
    let selected = state.selected_pull_request?;
    state
        .pull_requests
        .iter()
        .find(|pull_request| pull_request.number == selected)
}

fn selected_github_issue_in_state(
    state: &pohunek_gui_core::GitHubProviderState,
) -> Option<&providers::github::GitHubIssue> {
    let selected = state.selected_issue?;
    state.issues.iter().find(|issue| issue.number == selected)
}

fn filtered_pull_requests(
    state: &pohunek_gui_core::GitHubProviderState,
) -> impl Iterator<Item = &providers::github::GitHubPullRequest> {
    let search = state.search.trim().to_lowercase();
    state.pull_requests.iter().filter(move |pull_request| {
        search.is_empty()
            || pull_request.title.to_lowercase().contains(&search)
            || pull_request.number.to_string().contains(&search)
            || pull_request.head_ref_name.to_lowercase().contains(&search)
    })
}

fn filtered_github_issues(
    state: &pohunek_gui_core::GitHubProviderState,
) -> impl Iterator<Item = &providers::github::GitHubIssue> {
    let search = state.search.trim().to_lowercase();
    state.issues.iter().filter(move |issue| {
        search.is_empty()
            || issue.title.to_lowercase().contains(&search)
            || issue.number.to_string().contains(&search)
    })
}

fn linked_pr_status_label(host: &pohunek_gui_core::HostView, session: &SessionInfo) -> String {
    linked_github_status(host, session)
        .map(|status| format!("  [{status}]"))
        .unwrap_or_default()
}

fn linked_github_status(
    host: &pohunek_gui_core::HostView,
    session: &SessionInfo,
) -> Option<String> {
    let link = session_link_metadata(session)?;
    if link.provider != SessionLinkProvider::GitHub || link.kind != SessionLinkKind::PullRequest {
        return None;
    }
    let scope = session
        .project_id
        .as_ref()
        .and_then(|project_id| host.projects.get(project_id))
        .map(GitHubProviderScope::from_project);
    let status_key = scope.map(|scope| GitHubPullRequestStatusKey::new(scope, link.url.clone()));
    Some(
        status_key
            .as_ref()
            .and_then(|key| host.provider.github.pull_request_statuses.get(key))
            .map_or_else(|| "pr status unknown".to_owned(), format_pr_status),
    )
}

fn format_pr_status(status: &providers::github::PullRequestStatus) -> String {
    let review = review_label(&status.review_decision);
    let summary = providers::github::CheckSummary::from_checks(&status.checks);
    format!(
        "review={review} checks={} pass/{} fail/{} pending",
        summary.passed, summary.failed, summary.pending
    )
}

/// Short human label for a review decision.
fn review_label(decision: &providers::github::ReviewDecision) -> &str {
    match decision {
        providers::github::ReviewDecision::Approved => "approved",
        providers::github::ReviewDecision::ChangesRequested => "changes requested",
        providers::github::ReviewDecision::ReviewRequired => "review required",
        providers::github::ReviewDecision::None => "no review",
        providers::github::ReviewDecision::Unknown(value) => value.as_str(),
    }
}

/// Semantic background tone for a status pill.
#[derive(Debug, Clone, Copy)]
enum PillTone {
    Success,
    Danger,
    Warning,
    Neutral,
}

/// Muted text style for secondary row metadata.
fn muted_style(theme: &Theme) -> iced::widget::text::Style {
    // Dim the foreground text (not a background-derived gray, which is nearly
    // invisible on dark themes) so metadata stays clearly legible.
    let mut color = theme.extended_palette().background.base.text;
    color.a = 0.75;
    iced::widget::text::Style { color: Some(color) }
}

/// A small rounded status pill backed by a themed semantic color.
fn status_pill(label: impl Into<String>, tone: PillTone) -> Element<'static, Message> {
    let label = label.into();
    container(text(label).size(11))
        .padding([1, 6])
        .style(move |theme: &Theme| {
            let palette = theme.extended_palette();
            let pair = match tone {
                PillTone::Success => palette.success.weak,
                PillTone::Danger => palette.danger.weak,
                PillTone::Warning => palette.warning.weak,
                PillTone::Neutral => palette.secondary.weak,
            };
            iced::widget::container::Style {
                background: Some(Background::Color(pair.color)),
                text_color: Some(pair.text),
                border: iced::border::rounded(4.0),
                ..iced::widget::container::Style::default()
            }
        })
        .into()
}

/// A pill summarizing the pull request review decision.
fn review_pill(decision: &providers::github::ReviewDecision) -> Element<'static, Message> {
    use providers::github::ReviewDecision;
    let (label, tone) = match decision {
        ReviewDecision::Approved => ("review ok", PillTone::Success),
        ReviewDecision::ChangesRequested => ("changes req", PillTone::Danger),
        ReviewDecision::ReviewRequired => ("review req", PillTone::Warning),
        ReviewDecision::None => ("no review", PillTone::Neutral),
        ReviewDecision::Unknown(value) => (value.as_str(), PillTone::Neutral),
    };
    status_pill(label.to_owned(), tone)
}

/// A pill summarizing CI checks as `pass/fail/pending` counts.
fn ci_pill(checks: &[providers::github::CheckRun]) -> Element<'static, Message> {
    use providers::github::CiState;
    let summary = providers::github::CheckSummary::from_checks(checks);
    if summary.total() == 0 {
        return status_pill("no CI", PillTone::Neutral);
    }
    let tone = match summary.state() {
        CiState::Passing => PillTone::Success,
        CiState::Failing => PillTone::Danger,
        CiState::Pending => PillTone::Warning,
        CiState::None => PillTone::Neutral,
    };
    status_pill(
        format!(
            "CI {}/{}/{}",
            summary.passed, summary.failed, summary.pending
        ),
        tone,
    )
}

/// A label pill colored with GitHub's hex color when one is available.
fn label_pill(label: &providers::github::GitHubLabel) -> Element<'static, Message> {
    let name = label.name.clone();
    match color_from_hex(&label.color) {
        Some(background) => {
            let foreground = contrast_text_color(background);
            container(text(name).size(11))
                .padding([1, 6])
                .style(move |_theme: &Theme| iced::widget::container::Style {
                    background: Some(Background::Color(background)),
                    text_color: Some(foreground),
                    border: iced::border::rounded(4.0),
                    ..iced::widget::container::Style::default()
                })
                .into()
        }
        None => status_pill(name, PillTone::Neutral),
    }
}

/// Parses a 6-digit hex color (optional leading `#`) into an opaque color.
fn color_from_hex(hex: &str) -> Option<Color> {
    let hex = hex.strip_prefix('#').unwrap_or(hex);
    if hex.len() != 6 {
        return None;
    }
    let red = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let green = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let blue = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::from_rgb8(red, green, blue))
}

/// Extracts the `YYYY-MM-DD` date from an RFC 3339 timestamp.
fn date_part(timestamp: &str) -> &str {
    timestamp
        .split_once('T')
        .map_or(timestamp, |(date, _)| date)
}

/// Chooses black or white text for legibility on `background`.
fn contrast_text_color(background: Color) -> Color {
    // Perceived luminance (Rec. 601 weights), the same heuristic GitHub uses to
    // pick black-on-light vs white-on-dark label text.
    let luminance = 0.299 * background.r + 0.587 * background.g + 0.114 * background.b;
    // 0.6 keeps mid-tone labels (such as GitHub's yellow) on black text.
    if luminance > 0.6 {
        Color::BLACK
    } else {
        Color::WHITE
    }
}

/// A two-line pull request row: a title line and a metadata line.
///
/// The draft badge leads the title so it stays visible when the title wraps.
/// On the metadata line, fixed-size chips (review, CI, labels) come first and
/// the free-text fields (author, branch, diff, date) trail — so if the narrow
/// panel clips, it clips the least-critical text rather than the status chips.
fn pull_request_row(
    pull_request: &providers::github::GitHubPullRequest,
    selected: bool,
) -> Element<'_, Message> {
    let number = text(format!("#{}", pull_request.number))
        .size(13)
        .style(muted_style);
    let title = text(pull_request.title.as_str()).size(13);
    let title_line = if pull_request.is_draft {
        row![status_pill("draft", PillTone::Neutral), number, title]
    } else {
        row![number, title]
    }
    .spacing(8)
    .align_y(Center);

    let mut meta_line = row![
        review_pill(&pull_request.review_decision),
        ci_pill(&pull_request.checks),
    ]
    .spacing(6)
    .align_y(Center);
    for label in &pull_request.labels {
        meta_line = meta_line.push(label_pill(label));
    }

    let mut parts: Vec<String> = Vec::new();
    if let Some(author) = &pull_request.author {
        parts.push(format!("@{author}"));
    }
    parts.push(pull_request.head_ref_name.clone());
    if pull_request.additions > 0 || pull_request.deletions > 0 {
        parts.push(format!(
            "+{}/-{}",
            pull_request.additions, pull_request.deletions
        ));
    }
    if let Some(updated) = &pull_request.updated_at {
        parts.push(date_part(updated).to_owned());
    }
    if !parts.is_empty() {
        meta_line = meta_line.push(text(parts.join("  ·  ")).size(11).style(muted_style));
    }

    list_button(
        column![title_line, meta_line].spacing(4),
        Message::OpenGitHubPullRequest(pull_request.number),
        selected,
    )
}

fn toast_view(toast: &Toast) -> Element<'_, Message> {
    container(text(format!(
        "{} / {}: {}",
        toast.host_id, toast.session_id.0, toast.message
    )))
    .padding(8)
    .into()
}

fn selected_session(app: &PohunekApp) -> Option<(&HostId, &SessionInfo)> {
    let Some(Selection::Session {
        host_id,
        session_id,
    }) = app.ui_state.selection.as_ref()
    else {
        return None;
    };
    app.workspace
        .hosts
        .get_key_value(host_id)
        .and_then(|(host_id, host)| {
            host.sessions
                .get(&session_id.0)
                .map(|session| (host_id, session))
        })
}

fn selected_project(app: &PohunekApp) -> Option<(&HostId, &ProjectInfo)> {
    let Some(Selection::Project {
        host_id,
        project_id,
    }) = app.ui_state.selection.as_ref()
    else {
        return None;
    };
    app.workspace
        .hosts
        .get_key_value(host_id)
        .and_then(|(host_id, host)| {
            host.projects
                .get(project_id)
                .map(|project| (host_id, project))
        })
}

fn conn_label(conn: &ConnState) -> &'static str {
    match conn {
        ConnState::Connecting => "connecting",
        ConnState::Connected => "connected",
        ConnState::Disconnected => "disconnected",
        ConnState::Unreachable => "unreachable",
    }
}

/// U+25CF BLACK CIRCLE: a compact filled status dot that renders consistently
/// across desktop fonts.
const STATUS_DOT: &str = "\u{25CF}";

/// Themed color for an agent-activity status dot: working=success (green),
/// blocked=danger (red), idle=secondary (muted), unknown=dim background.
fn activity_color(theme: &Theme, activity: Option<AgentActivity>) -> iced::Color {
    let palette = theme.extended_palette();
    match activity {
        Some(AgentActivity::Working) => palette.success.base.color,
        Some(AgentActivity::Blocked) => palette.danger.base.color,
        Some(AgentActivity::Idle) => palette.secondary.base.color,
        None => palette.background.strong.color,
    }
}

/// A filled-circle indicator colored by agent activity.
fn status_dot(activity: Option<AgentActivity>) -> Element<'static, Message> {
    text(STATUS_DOT)
        .size(13)
        .style(move |theme: &Theme| iced::widget::text::Style {
            color: Some(activity_color(theme, activity)),
        })
        .into()
}

/// Themed color for a host connection dot: connected=success, connecting=warning,
/// disconnected/unreachable=danger.
fn conn_color(theme: &Theme, conn: &ConnState) -> iced::Color {
    let palette = theme.extended_palette();
    match conn {
        ConnState::Connected => palette.success.base.color,
        ConnState::Connecting => palette.warning.base.color,
        ConnState::Disconnected | ConnState::Unreachable => palette.danger.base.color,
    }
}

/// A filled-circle indicator colored by host connection state.
fn conn_dot(conn: ConnState) -> Element<'static, Message> {
    text(STATUS_DOT)
        .size(13)
        .style(move |theme: &Theme| iced::widget::text::Style {
            color: Some(conn_color(theme, &conn)),
        })
        .into()
}

fn theme(_app: &PohunekApp) -> Theme {
    Theme::TokyoNight
}

#[derive(Debug, Default)]
struct ShellAttachSpawner;

impl AttachCommandSpawner for ShellAttachSpawner {
    fn spawn(&mut self, command: &str) -> Result<(), String> {
        Command::new("sh")
            .arg("-c")
            .arg(command)
            .spawn()
            .map(|_| ())
            .map_err(|err| format!("failed to spawn attach command `{command}`: {err}"))
    }
}

fn spawn_attach(template: &str, values: &AttachTemplateValues) -> Result<(), String> {
    let mut spawner = ShellAttachSpawner;
    spawn_attach_command(&mut spawner, template, values).map(|_| ())
}

/// Build the task that opens a session in a terminal.
///
/// Live sessions spawn the configured attach command immediately. Terminal
/// sessions first ask the daemon to relaunch from native resume metadata; the
/// command-completion path then calls this again and attaches to the live PTY.
fn attach_task(
    app: &PohunekApp,
    host_id: &HostId,
    session_id: &SessionId,
) -> Result<Task<Message>, String> {
    if session_requires_resume_before_attach(app, host_id, session_id) {
        return resume_session_task(app, host_id, session_id);
    }

    let (template, values) = app.attach_values(host_id, session_id)?;
    Ok(Task::perform(
        async move { spawn_attach(&template, &values) },
        Message::AttachSpawned,
    ))
}

fn session_requires_resume_before_attach(
    app: &PohunekApp,
    host_id: &HostId,
    session_id: &SessionId,
) -> bool {
    app.workspace
        .hosts
        .get(host_id)
        .and_then(|host| host.sessions.get(&session_id.0))
        .is_some_and(|session| session.state.is_terminal())
}

fn spawn_notification(command: &str, intent: &NotificationIntent) -> Result<(), String> {
    Command::new(command)
        .arg(&intent.title)
        .arg(&intent.body)
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("failed to spawn notification command `{command}`: {err}"))
}

fn window_dimension_to_f32(value: u32) -> f32 {
    f32::from(u16::try_from(value).unwrap_or(u16::MAX))
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Iced reports positive window pixel sizes as f32; UI state persists integer pixels"
)]
fn window_dimension_to_u32(value: f32) -> u32 {
    value.round().clamp(1.0, f32::from(u16::MAX)) as u32
}

#[derive(Debug, Clone)]
struct AppConfig {
    attach_command: String,
    pohunek_bin: String,
    local_host: HostConfig,
    connection_options: ConnectionOptions,
    terminal_size: TerminalSize,
    notification_command: String,
    providers: ProviderAppConfig,
}

impl AppConfig {
    fn load() -> Result<Self, ConfigError> {
        let config_dir = config_dir()?;
        let path = config_dir.join("gui.toml");
        let raw = std::fs::read_to_string(&path).map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })?;
        let raw: RawConfig = toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.clone(),
            source,
        })?;
        let local_host = HostConfig::local("local", local_socket_path()?);
        let raw_gui = raw.gui.unwrap_or_default();
        let connection_options = raw_gui.connection_options()?;
        let terminal_size = raw_gui.terminal_size()?;
        Ok(Self {
            attach_command: raw.attach_command,
            pohunek_bin: raw.pohunek_bin,
            local_host,
            connection_options,
            terminal_size,
            notification_command: raw
                .notification_command
                .unwrap_or_else(|| DEFAULT_NOTIFICATION_COMMAND.to_owned()),
            providers: raw.providers.unwrap_or_default().into_provider_config()?,
        })
    }
}

#[derive(Debug, Clone, Default)]
struct ProviderAppConfig {
    linear: Option<LinearAppConfig>,
    github: Option<GitHubAppConfig>,
    /// Host-layer (`gui.toml`) named filters, merged with the project layer and
    /// built-in defaults when the provider panels resolve their pickers.
    filters: providers::filters::ProviderFilterSet,
}

#[derive(Debug, Clone)]
struct LinearAppConfig {
    token_key: String,
    endpoint: String,
    token_lookup_timeout: Duration,
}

#[derive(Debug, Clone)]
struct GitHubAppConfig {
    gh_bin: PathBuf,
    timeout: Duration,
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    attach_command: String,
    pohunek_bin: String,
    #[serde(default)]
    notification_command: Option<String>,
    #[serde(default)]
    gui: Option<RawGuiConfig>,
    #[serde(default)]
    providers: Option<RawProvidersConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct RawGuiConfig {
    #[serde(default)]
    connect_timeout_ms: Option<u64>,
    #[serde(default)]
    request_timeout_ms: Option<u64>,
    #[serde(default)]
    reconcile_secs: Option<u64>,
    #[serde(default)]
    backoff_initial_ms: Option<u64>,
    #[serde(default)]
    backoff_max_ms: Option<u64>,
    #[serde(default)]
    terminal_cols: Option<u16>,
    #[serde(default)]
    terminal_rows: Option<u16>,
}

#[derive(Debug, Default, Deserialize)]
struct RawProvidersConfig {
    #[serde(default)]
    linear: Option<RawLinearProviderConfig>,
    #[serde(default)]
    github: Option<RawGitHubProviderConfig>,
}

#[derive(Debug, Deserialize)]
struct RawLinearProviderConfig {
    token_key: String,
    endpoint: String,
    token_timeout_ms: u64,
    #[serde(default)]
    filters: Vec<RawLinearFilter>,
}

#[derive(Debug, Deserialize)]
struct RawGitHubProviderConfig {
    gh_bin: PathBuf,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    filters: Vec<RawGitHubFilter>,
}

/// One `[providers.github]` (or in-repo) pull request filter as written in TOML.
#[derive(Debug, Deserialize)]
struct RawGitHubFilter {
    name: String,
    #[serde(default)]
    search: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

/// One `[providers.linear]` (or in-repo) issue filter as written in TOML.
#[derive(Debug, Deserialize)]
struct RawLinearFilter {
    name: String,
    /// Raw Linear `IssueFilter` as a TOML table, converted to JSON at load time.
    filter: toml::Value,
}

/// In-repo `<repo_root>/.pohunek/providers.toml` filter layer.
#[derive(Debug, Default, Deserialize)]
struct RawProjectFilters {
    #[serde(default)]
    github: Vec<RawGitHubFilter>,
    #[serde(default)]
    linear: Vec<RawLinearFilter>,
}

impl RawGitHubFilter {
    fn into_filter(self) -> Result<providers::filters::GitHubFilter, ConfigError> {
        let name = non_empty_config_value(self.name, "providers.github.filters[].name")?;
        let state = match self.state {
            Some(state) => providers::filters::GitHubPrState::parse(&state).map_err(|source| {
                ConfigError::ProviderFilter {
                    message: source.to_string(),
                }
            })?,
            None => providers::filters::GitHubPrState::default(),
        };
        Ok(providers::filters::GitHubFilter::new(
            name,
            self.search.unwrap_or_default(),
            state,
        ))
    }
}

impl RawLinearFilter {
    fn into_filter(self) -> Result<providers::filters::LinearFilter, ConfigError> {
        let name = non_empty_config_value(self.name, "providers.linear.filters[].name")?;
        let filter =
            serde_json::to_value(self.filter).map_err(|source| ConfigError::ProviderFilter {
                message: format!("invalid Linear filter `{name}`: {source}"),
            })?;
        Ok(providers::filters::LinearFilter::new(name, filter))
    }
}

impl RawProjectFilters {
    fn into_filter_set(self) -> Result<providers::filters::ProviderFilterSet, ConfigError> {
        Ok(providers::filters::ProviderFilterSet {
            github: self
                .github
                .into_iter()
                .map(RawGitHubFilter::into_filter)
                .collect::<Result<_, _>>()?,
            linear: self
                .linear
                .into_iter()
                .map(RawLinearFilter::into_filter)
                .collect::<Result<_, _>>()?,
        })
    }
}

impl RawProvidersConfig {
    fn into_provider_config(self) -> Result<ProviderAppConfig, ConfigError> {
        let mut filters = providers::filters::ProviderFilterSet::default();
        let linear = match self.linear {
            Some(raw) => {
                let (config, linear_filters) = raw.into_app_config()?;
                filters.linear = linear_filters;
                Some(config)
            }
            None => None,
        };
        let github = match self.github {
            Some(raw) => {
                let (config, github_filters) = raw.into_app_config()?;
                filters.github = github_filters;
                Some(config)
            }
            None => None,
        };
        Ok(ProviderAppConfig {
            linear,
            github,
            filters,
        })
    }
}

impl RawLinearProviderConfig {
    fn into_app_config(
        self,
    ) -> Result<(LinearAppConfig, Vec<providers::filters::LinearFilter>), ConfigError> {
        let filters = self
            .filters
            .into_iter()
            .map(RawLinearFilter::into_filter)
            .collect::<Result<Vec<_>, _>>()?;
        let config = LinearAppConfig {
            token_key: non_empty_config_value(self.token_key, "providers.linear.token_key")?,
            endpoint: validate_http_endpoint(self.endpoint, "providers.linear.endpoint")?,
            token_lookup_timeout: required_duration_millis(
                self.token_timeout_ms,
                "providers.linear.token_timeout_ms",
            )?,
        };
        Ok((config, filters))
    }
}

impl RawGitHubProviderConfig {
    fn into_app_config(
        self,
    ) -> Result<(GitHubAppConfig, Vec<providers::filters::GitHubFilter>), ConfigError> {
        let filters = self
            .filters
            .into_iter()
            .map(RawGitHubFilter::into_filter)
            .collect::<Result<Vec<_>, _>>()?;
        let config = GitHubAppConfig {
            gh_bin: non_empty_config_path(self.gh_bin, "providers.github.gh_bin")?,
            timeout: duration_millis(
                self.timeout_ms,
                "providers.github.timeout_ms",
                Duration::from_secs(20),
            )?,
        };
        Ok((config, filters))
    }
}

impl RawGuiConfig {
    fn connection_options(&self) -> Result<ConnectionOptions, ConfigError> {
        let defaults = ConnectionOptions::default();
        Ok(ConnectionOptions {
            connect_timeout: duration_millis(
                self.connect_timeout_ms,
                "gui.connect_timeout_ms",
                defaults.connect_timeout,
            )?,
            request_timeout: duration_millis(
                self.request_timeout_ms,
                "gui.request_timeout_ms",
                defaults.request_timeout,
            )?,
            reconcile_interval: duration_secs(
                self.reconcile_secs,
                "gui.reconcile_secs",
                defaults.reconcile_interval,
            )?,
            backoff_initial: duration_millis(
                self.backoff_initial_ms,
                "gui.backoff_initial_ms",
                defaults.backoff_initial,
            )?,
            backoff_max: duration_millis(
                self.backoff_max_ms,
                "gui.backoff_max_ms",
                defaults.backoff_max,
            )?,
        })
    }

    fn terminal_size(&self) -> Result<TerminalSize, ConfigError> {
        Ok(TerminalSize {
            cols: terminal_dimension(
                self.terminal_cols,
                "gui.terminal_cols",
                DEFAULT_TERMINAL_COLS,
            )?,
            rows: terminal_dimension(
                self.terminal_rows,
                "gui.terminal_rows",
                DEFAULT_TERMINAL_ROWS,
            )?,
        })
    }
}

#[derive(Debug, Error)]
enum ConfigError {
    #[error("missing environment variable `{var}`")]
    MissingEnv { var: String },
    #[error("failed to read `{}`: {source}", path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse `{}`: {source}", path.display())]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("invalid `{field}`: {message}")]
    Invalid {
        field: &'static str,
        message: String,
    },
    #[error("invalid provider filter: {message}")]
    ProviderFilter { message: String },
}

fn non_empty_config_value(value: String, field: &'static str) -> Result<String, ConfigError> {
    if value.trim().is_empty() {
        Err(ConfigError::Invalid {
            field,
            message: "must not be empty".to_owned(),
        })
    } else {
        Ok(value)
    }
}

fn non_empty_config_path(value: PathBuf, field: &'static str) -> Result<PathBuf, ConfigError> {
    if value.as_os_str().is_empty() {
        Err(ConfigError::Invalid {
            field,
            message: "must not be empty".to_owned(),
        })
    } else if value.components().count() > 1 && !value.exists() {
        Err(ConfigError::Invalid {
            field,
            message: "path does not exist".to_owned(),
        })
    } else {
        Ok(value)
    }
}

fn validate_http_endpoint(value: String, field: &'static str) -> Result<String, ConfigError> {
    let value = non_empty_config_value(value, field)?;
    let Some(rest) = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
    else {
        return Err(ConfigError::Invalid {
            field,
            message: "must start with http:// or https://".to_owned(),
        });
    };
    if rest.split('/').next().is_none_or(str::is_empty) {
        return Err(ConfigError::Invalid {
            field,
            message: "must include a host".to_owned(),
        });
    }
    Ok(value)
}

fn duration_millis(
    value: Option<u64>,
    field: &'static str,
    default: Duration,
) -> Result<Duration, ConfigError> {
    value.map_or(Ok(default), |millis| {
        if millis == 0 {
            Err(ConfigError::Invalid {
                field,
                message: "must be greater than zero".to_owned(),
            })
        } else {
            Ok(Duration::from_millis(millis))
        }
    })
}

fn required_duration_millis(value: u64, field: &'static str) -> Result<Duration, ConfigError> {
    if value == 0 {
        Err(ConfigError::Invalid {
            field,
            message: "must be greater than zero".to_owned(),
        })
    } else {
        Ok(Duration::from_millis(value))
    }
}

fn terminal_dimension(
    value: Option<u16>,
    field: &'static str,
    default: u16,
) -> Result<u16, ConfigError> {
    value.map_or(Ok(default), |dimension| {
        if dimension == 0 {
            Err(ConfigError::Invalid {
                field,
                message: "must be greater than zero".to_owned(),
            })
        } else {
            Ok(dimension)
        }
    })
}

fn duration_secs(
    value: Option<u64>,
    field: &'static str,
    default: Duration,
) -> Result<Duration, ConfigError> {
    value.map_or(Ok(default), |secs| {
        if secs == 0 {
            Err(ConfigError::Invalid {
                field,
                message: "must be greater than zero".to_owned(),
            })
        } else {
            Ok(Duration::from_secs(secs))
        }
    })
}

fn local_socket_path() -> Result<PathBuf, ConfigError> {
    pohunek_paths::socket_path().map_err(config_path_error)
}

fn config_dir() -> Result<PathBuf, ConfigError> {
    pohunek_paths::config_home()
        .map(|home| home.join(pohunek_paths::APP_DIR))
        .map_err(config_path_error)
}

fn config_path_error(err: pohunek_paths::PathError) -> ConfigError {
    match err {
        pohunek_paths::PathError::MissingEnv { var } => ConfigError::MissingEnv { var },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_agent_choices_include_shell_runtime() {
        assert_eq!(
            AgentChoice::ALL,
            [AgentChoice::Shell, AgentChoice::Codex, AgentChoice::Claude]
        );
        assert_eq!(AgentChoice::Shell.as_str(), "shell");
        assert_eq!(AgentChoice::from_wire("shell"), AgentChoice::Shell);
    }

    #[test]
    fn notification_timestamp_parser_handles_epoch_and_leap_day() {
        assert_eq!(parse_rfc3339_utc_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_utc_seconds("1970-01-02T00:00:01Z"),
            Some(SECONDS_PER_DAY + 1)
        );
        assert!(parse_rfc3339_utc_seconds("2024-02-29T12:00:00Z").is_some());
    }

    #[test]
    fn notification_timestamp_parser_rejects_invalid_dates() {
        assert_eq!(parse_rfc3339_utc_seconds("2023-02-29T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_utc_seconds("2026-01-01T24:00:00Z"), None);
        assert_eq!(parse_rfc3339_utc_seconds("not-a-timestamp"), None);
    }

    fn test_session(id: &str, state: protocol::SessionState) -> SessionInfo {
        SessionInfo {
            name: None,
            id: SessionId(id.to_owned()),
            agent: "codex".to_owned(),
            agent_base: protocol::AgentKind::Codex,
            cwd: PathBuf::from("/tmp/project"),
            pid: 42,
            cols: 80,
            rows: 24,
            state,
            state_source: protocol::StateSource::Process,
            activity: None,
            active_agent: None,
            active_agent_base: None,
            active_agent_session_id: None,
            active_agent_session_path: None,
            native_session_id: Some("native-1".to_owned()),
            native_session_path: None,
            project_id: None,
            project_label: None,
            metadata: BTreeMap::new(),
            is_linked_worktree: Some(false),
            repo: None,
            branch: None,
            worktree_path: None,
            warnings: Vec::new(),
            created_at: "2026-06-29T00:00:00Z".to_owned(),
            updated_at: "2026-06-29T00:00:00Z".to_owned(),
            exit_code: None,
        }
    }

    fn app_with_session(host_id: &HostId, session: SessionInfo) -> PohunekApp {
        let mut host = pohunek_gui_core::HostView {
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
        };
        host.sessions.insert(session.id.0.clone(), session);

        let mut app = PohunekApp {
            workspace: Workspace::default(),
            config: Err("test config is intentionally absent".to_owned()),
            hosts: Vec::new(),
            ui_state: UiState::default(),
            start: StartForm::default(),
            assistant: AssistantForm::default(),
            prompt_editor: text_editor::Content::new(),
            assistant_editor: text_editor::Content::new(),
            template_recipe: None,
            modal: ModalView::None,
            activity_filter: None,
            notification_filter: NotificationFilter::default(),
            inbox_open: false,
            metadata_edit: MetadataEdit::default(),
            rename_edit: String::new(),
            project_edit: ProjectEdit::default(),
            selected_action: None,
            project_filters: BTreeMap::new(),
            last_session_click: None,
            state_dir: None,
            status: None,
            notified_intents: 0,
        };
        app.workspace.hosts.insert(host_id.clone(), host);
        app
    }

    #[test]
    fn terminal_session_resumes_before_attach() {
        let host_id = HostId::new("local");
        let stopped = test_session("s-1", protocol::SessionState::Stopped);
        let running = test_session("s-2", protocol::SessionState::Running);

        assert!(session_requires_resume_before_attach(
            &app_with_session(&host_id, stopped.clone()),
            &host_id,
            &stopped.id
        ));
        assert!(!session_requires_resume_before_attach(
            &app_with_session(&host_id, running.clone()),
            &host_id,
            &running.id
        ));
    }

    #[test]
    fn selected_project_identity_ignores_manual_project_reference() {
        let host_id = HostId::new("local");
        let project = ProjectInfo {
            id: "selected-project".to_owned(),
            label: "Selected project".to_owned(),
            repo_root: PathBuf::from("/tmp/selected-project"),
            git_common_dir: PathBuf::from("/tmp/selected-project/.git"),
            origin_url: None,
            default_base_branch: None,
            source: protocol::ProjectSource::Manual,
            is_bare: false,
            added_at: "2026-06-29T00:00:00Z".to_owned(),
            last_used_at: "2026-06-29T00:00:00Z".to_owned(),
        };
        let mut host = pohunek_gui_core::HostView {
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
        };
        host.projects.insert(project.id.clone(), project.clone());

        let mut app = PohunekApp {
            workspace: Workspace::default(),
            config: Err("test config is intentionally absent".to_owned()),
            hosts: Vec::new(),
            ui_state: UiState::default(),
            start: StartForm::default(),
            assistant: AssistantForm::default(),
            prompt_editor: text_editor::Content::new(),
            assistant_editor: text_editor::Content::new(),
            template_recipe: None,
            modal: ModalView::None,
            activity_filter: None,
            notification_filter: NotificationFilter::default(),
            inbox_open: false,
            metadata_edit: MetadataEdit::default(),
            rename_edit: String::new(),
            project_edit: ProjectEdit {
                reference: "manual-project".to_owned(),
                ..ProjectEdit::default()
            },
            selected_action: None,
            project_filters: BTreeMap::new(),
            last_session_click: None,
            state_dir: None,
            status: None,
            notified_intents: 0,
        };
        app.workspace.hosts.insert(host_id.clone(), host);
        app.ui_state.selection = Some(Selection::Project {
            host_id: host_id.clone(),
            project_id: project.id.clone(),
        });
        app.workspace.selection.clone_from(&app.ui_state.selection);

        let (project_id, repo_root) = selected_project_identity(&app).expect("selected project");

        assert_eq!(project_id, project.id);
        assert_eq!(repo_root, project.repo_root);
        assert_eq!(
            selected_project_reference(&app).expect("manual reference"),
            "manual-project"
        );
    }

    #[test]
    fn selected_assistant_project_uses_session_project() {
        let host_id = HostId::new("local");
        let project = ProjectInfo {
            id: "selected-project".to_owned(),
            label: "Selected project".to_owned(),
            repo_root: PathBuf::from("/tmp/selected-project"),
            git_common_dir: PathBuf::from("/tmp/selected-project/.git"),
            origin_url: None,
            default_base_branch: None,
            source: protocol::ProjectSource::Manual,
            is_bare: false,
            added_at: "2026-06-29T00:00:00Z".to_owned(),
            last_used_at: "2026-06-29T00:00:00Z".to_owned(),
        };
        let session = SessionInfo {
            name: None,
            id: SessionId("s-1".to_owned()),
            agent: "codex".to_owned(),
            agent_base: protocol::AgentKind::Codex,
            cwd: PathBuf::from("/tmp/selected-project"),
            pid: 42,
            cols: 80,
            rows: 24,
            state: protocol::SessionState::Running,
            state_source: protocol::StateSource::Process,
            activity: None,
            active_agent: None,
            active_agent_base: None,
            active_agent_session_id: None,
            active_agent_session_path: None,
            native_session_id: None,
            native_session_path: None,
            project_id: Some(project.id.clone()),
            project_label: Some(project.label.clone()),
            metadata: BTreeMap::new(),
            is_linked_worktree: Some(false),
            repo: Some(project.repo_root.clone()),
            branch: Some("main".to_owned()),
            worktree_path: Some(project.repo_root.clone()),
            warnings: Vec::new(),
            created_at: "2026-06-29T00:00:00Z".to_owned(),
            updated_at: "2026-06-29T00:00:00Z".to_owned(),
            exit_code: None,
        };
        let mut host = pohunek_gui_core::HostView {
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
        };
        host.projects.insert(project.id.clone(), project.clone());
        host.sessions.insert(session.id.0.clone(), session.clone());

        let mut app = PohunekApp {
            workspace: Workspace::default(),
            config: Err("test config is intentionally absent".to_owned()),
            hosts: Vec::new(),
            ui_state: UiState::default(),
            start: StartForm::default(),
            assistant: AssistantForm::default(),
            prompt_editor: text_editor::Content::new(),
            assistant_editor: text_editor::Content::new(),
            template_recipe: None,
            modal: ModalView::None,
            activity_filter: None,
            notification_filter: NotificationFilter::default(),
            inbox_open: false,
            metadata_edit: MetadataEdit::default(),
            rename_edit: String::new(),
            project_edit: ProjectEdit::default(),
            selected_action: None,
            project_filters: BTreeMap::new(),
            last_session_click: None,
            state_dir: None,
            status: None,
            notified_intents: 0,
        };
        app.workspace.hosts.insert(host_id.clone(), host);
        app.hosts.push(HostConfig::local(
            "local",
            PathBuf::from("/tmp/pohunek.sock"),
        ));
        app.ui_state.selection = Some(Selection::Session {
            host_id: host_id.clone(),
            session_id: session.id.clone(),
        });
        app.workspace.selection.clone_from(&app.ui_state.selection);

        let target = selected_assistant_project(&app).expect("session project target");

        assert_eq!(target.host.id, host_id);
        assert_eq!(target.project_ref, project.id);
    }

    #[test]
    fn provider_config_rejects_linear_endpoint_without_http_scheme() {
        let err = validate_http_endpoint(
            "linear.example/graphql".to_owned(),
            "providers.linear.endpoint",
        )
        .expect_err("endpoint without scheme");

        assert!(err
            .to_string()
            .contains("must start with http:// or https://"));
    }

    #[test]
    fn gui_manifest_uses_wayland_only_iced_features() {
        let manifest: toml::Value =
            toml::from_str(include_str!("../Cargo.toml")).expect("gui manifest parses");
        let iced = manifest
            .get("dependencies")
            .and_then(toml::Value::as_table)
            .and_then(|dependencies| dependencies.get("iced"))
            .expect("iced dependency");
        let features = iced
            .get("features")
            .and_then(toml::Value::as_array)
            .expect("explicit iced features");

        assert_eq!(
            iced.get("default-features").and_then(toml::Value::as_bool),
            Some(false)
        );
        assert!(features
            .iter()
            .any(|feature| feature.as_str() == Some("wayland")));
        assert!(!features
            .iter()
            .any(|feature| feature.as_str() == Some("x11")));
    }

    #[test]
    fn workspace_iced_dependency_disables_default_features() {
        let manifest: toml::Value =
            toml::from_str(include_str!("../../../Cargo.toml")).expect("workspace manifest parses");
        let iced = manifest
            .get("workspace")
            .and_then(toml::Value::as_table)
            .and_then(|workspace| workspace.get("dependencies"))
            .and_then(toml::Value::as_table)
            .and_then(|dependencies| dependencies.get("iced"))
            .and_then(toml::Value::as_table)
            .expect("workspace iced dependency config");

        assert_eq!(
            iced.get("default-features").and_then(toml::Value::as_bool),
            Some(false)
        );
    }

    #[test]
    fn wayland_guard_accepts_nonempty_wayland_display() {
        validate_wayland_display(Some(std::ffi::OsStr::new("wayland-1")), None)
            .expect("wayland display");
    }

    #[test]
    fn wayland_guard_rejects_missing_wayland_display() {
        let err = validate_wayland_display(None, None).expect_err("missing wayland display");

        assert!(err.to_string().contains("WAYLAND_DISPLAY"));
        assert!(err.to_string().contains("Wayland-only"));
    }

    #[test]
    fn wayland_guard_rejects_empty_wayland_display_with_x11_hint() {
        let err = validate_wayland_display(
            Some(std::ffi::OsStr::new("")),
            Some(std::ffi::OsStr::new(":0")),
        )
        .expect_err("empty wayland display");

        assert!(err.to_string().contains("DISPLAY is set"));
        assert!(err.to_string().contains("X11 is not supported"));
    }

    #[test]
    fn provider_config_rejects_zero_linear_token_timeout() {
        let err = RawLinearProviderConfig {
            token_key: "linear-token-ref".to_owned(),
            endpoint: "https://linear.example/graphql".to_owned(),
            token_timeout_ms: 0,
            filters: Vec::new(),
        }
        .into_app_config()
        .expect_err("zero token timeout");

        assert!(err.to_string().contains("must be greater than zero"));
    }

    #[test]
    fn gui_config_rejects_zero_terminal_columns() {
        let err = RawGuiConfig {
            terminal_cols: Some(0),
            ..RawGuiConfig::default()
        }
        .terminal_size()
        .expect_err("zero terminal columns");

        assert!(err.to_string().contains("must be greater than zero"));
    }

    #[test]
    fn gui_config_accepts_custom_terminal_size() {
        let size = RawGuiConfig {
            terminal_cols: Some(132),
            terminal_rows: Some(40),
            ..RawGuiConfig::default()
        }
        .terminal_size()
        .expect("custom terminal size");

        assert_eq!(size.cols, 132);
        assert_eq!(size.rows, 40);
    }

    #[test]
    fn provider_config_accepts_command_name_for_gh_bin() {
        let path = non_empty_config_path(PathBuf::from("gh"), "providers.github.gh_bin")
            .expect("command name");

        assert_eq!(path, PathBuf::from("gh"));
    }

    #[test]
    fn provider_config_rejects_missing_explicit_gh_path() {
        let missing =
            std::env::temp_dir().join(format!("pohunek-gui-missing-gh-bin-{}", std::process::id()));
        let err = non_empty_config_path(missing, "providers.github.gh_bin")
            .expect_err("missing explicit path");

        assert!(err.to_string().contains("path does not exist"));
    }
}
