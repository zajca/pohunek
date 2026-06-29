//! Native Iced shell for the pohunek control plane.

// Rust guideline compliant 2026-06-26
#![forbid(unsafe_code)]

mod runtime;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use iced::widget::{button, column, container, pick_list, row, scrollable, text, text_input};
use iced::{window, Background, Center, Element, Fill, Size, Subscription, Task, Theme};
use pohunek_gui_core::{
    add_project_with_options, create_session_with_options, default_state_dir, discover_hosts,
    inspect_session_with_options, launch_action_prompt_with_options,
    launch_provider_item_with_options, list_project_actions_with_options, preview_action_prompt,
    providers, remove_project_with_options, rename_project_with_options,
    resolve_project_action_with_options, session_link_metadata, session_metadata_rows,
    set_session_metadata_with_options, show_project_with_options, spawn_attach_command,
    stop_session_with_options, AttachCommandSpawner, AttachTemplateValues, ConnState,
    ConnectionOptions, GitHubProviderScope, GitHubPullRequestStatusKey, HostConfig, HostId,
    Message as CoreMessage, NotificationIntent, PromptLaunchParams, ProviderLaunchItem,
    ProviderLaunchParams, ProviderOperation, ProviderPanel, ProviderRequestId, Selection,
    SessionLinkKind, SessionLinkProvider, Toast, TreeNodeId, UiState, WindowSize, Workspace,
};
use protocol::{
    AgentActivity, ProjectActionParams, ProjectActionsParams, ProjectAddParams, ProjectInfo,
    ProjectRemoveParams, ProjectRenameParams, ProjectShowParams, ProviderKind, SessionId,
    SessionInfo, SessionNewParams, SessionSetMetadataParams,
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

pub fn main() -> iced::Result {
    let boot = BootState::load();
    let initial_window_size = boot.ui_state.window_size;
    iced::application(move || PohunekApp::boot(boot.clone()), update, view)
        .subscription(subscription)
        .theme(theme)
        .window_size((
            window_dimension_to_f32(initial_window_size.width),
            window_dimension_to_f32(initial_window_size.height),
        ))
        .run()
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

#[derive(Debug, Clone)]
struct PohunekApp {
    workspace: Workspace,
    config: Result<AppConfig, String>,
    hosts: Vec<HostConfig>,
    ui_state: UiState,
    start: StartForm,
    /// Active activity filter for the agents monitor; `None` shows all agents.
    activity_filter: Option<AgentActivity>,
    metadata_edit: MetadataEdit,
    project_edit: ProjectEdit,
    /// Action chosen in the provider browser for launching the selected item.
    selected_action: Option<String>,
    state_dir: Option<PathBuf>,
    status: Option<String>,
    notified_intents: usize,
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
                activity_filter: None,
                metadata_edit: MetadataEdit::default(),
                project_edit: ProjectEdit::default(),
                selected_action: None,
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

/// Agent the operator can launch from the GUI. Backed by the protocol
/// [`AgentKind`] wire strings; rendered in a `pick_list` instead of being typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentChoice {
    Codex,
    Claude,
}

impl AgentChoice {
    /// Selectable agents, in display order.
    const ALL: [Self; 2] = [Self::Codex, Self::Claude];

    /// Wire string passed verbatim to `session new --agent`.
    const fn as_str(self) -> &'static str {
        match self {
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
/// typed; only the agent, an optional initial input and (under Advanced) branch
/// overrides are operator-supplied.
#[derive(Debug, Clone)]
struct StartForm {
    agent: AgentChoice,
    /// Selected prompt template (a `None`-provider action name); `None` means a
    /// blank session whose input is the free-text field.
    template: Option<String>,
    input: String,
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
            template: None,
            input: String::new(),
            show_advanced: false,
            branch: String::new(),
            base_branch: String::new(),
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

#[derive(Debug, Clone)]
enum Message {
    Core(CoreMessage),
    HostsDiscovered(DiscoveryResult),
    ToggleNode(TreeNodeId),
    FilterActivity(Option<AgentActivity>),
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
    StartAgentSelected(AgentChoice),
    StartTemplateSelected(String),
    StartInputChanged(String),
    ToggleStartAdvanced,
    StartBranchChanged(String),
    StartBaseBranchChanged(String),
    CreateSession,
    InspectSelectedSession,
    StopSelectedSession,
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
    RemoveProject {
        prune_worktrees: bool,
    },
    SelectAction(String),
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
        Message::SelectSession {
            host_id,
            session_id,
        } => {
            app.workspace
                .select_session(host_id.clone(), session_id.clone());
            app.ui_state.selection = Some(Selection::Session {
                host_id,
                session_id,
            });
            tasks.push(save_ui_state_task(app));
        }
        Message::SelectProject {
            host_id,
            project_id,
        } => {
            app.workspace
                .select_project(host_id.clone(), project_id.clone());
            app.ui_state.selection = app.workspace.selection.clone();
            app.project_edit.reference = project_id;
            app.selected_action = None;
            app.start.template = None;
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
        } => match app.attach_values(&host_id, &session_id) {
            Ok((template, values)) => tasks.push(Task::perform(
                async move { spawn_attach(&template, &values) },
                Message::AttachSpawned,
            )),
            Err(err) => app.status = Some(err),
        },
        Message::StartAgentSelected(agent) => app.start.agent = agent,
        Message::StartTemplateSelected(template) => {
            app.start.template = (template != BLANK_TEMPLATE_LABEL).then_some(template);
        }
        Message::StartInputChanged(value) => app.start.input = value,
        Message::ToggleStartAdvanced => app.start.show_advanced = !app.start.show_advanced,
        Message::StartBranchChanged(value) => app.start.branch = value,
        Message::StartBaseBranchChanged(value) => app.start.base_branch = value,
        Message::CreateSession => {
            let result = match app.start.template.clone() {
                Some(action_name) => launch_template_session_task(app, action_name),
                None => create_session_task(app),
            };
            match result {
                Ok(task) => tasks.push(task),
                Err(err) => app.status = Some(err),
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
        Message::RemoveProject { prune_worktrees } => {
            match remove_project_task(app, prune_worktrees) {
                Ok(task) => tasks.push(task),
                Err(err) => app.status = Some(err),
            }
        }
        Message::SelectAction(name) => app.selected_action = Some(name),
        Message::FetchLinearIssues => match begin_linear_issues_request(app) {
            Ok(request_id) => push_provider_task_result(
                app,
                &mut tasks,
                SessionLinkProvider::Linear,
                ProviderOperation::LinearIssues,
                Some(request_id),
                fetch_linear_issues_task(app, request_id),
            ),
            Err(err) => app.status = Some(err),
        },
        Message::FetchGitHubPullRequests => match begin_github_pull_requests_request(app) {
            Ok(request_id) => push_provider_task_result(
                app,
                &mut tasks,
                SessionLinkProvider::GitHub,
                ProviderOperation::GitHubPullRequests,
                Some(request_id),
                fetch_github_pull_requests_task(app, request_id),
            ),
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
        Message::LaunchLinearIssue => push_provider_task_result(
            app,
            &mut tasks,
            SessionLinkProvider::Linear,
            ProviderOperation::Launch,
            None,
            launch_linear_issue_task(app),
        ),
        Message::LaunchGitHubPullRequest => push_provider_task_result(
            app,
            &mut tasks,
            SessionLinkProvider::GitHub,
            ProviderOperation::Launch,
            None,
            launch_github_pull_request_task(app),
        ),
        Message::CoreCommandCompleted(result) => match result {
            Ok(message) => app.workspace.apply(message),
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

fn create_session_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let host = selected_host_config(app)?;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    let project = selected_project_reference(app)?;
    let params = build_session_params(&app.start, project, terminal_size(app)?);
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

/// Creates a session from a prompt template: resolves the chosen `None`-provider
/// action, renders its static prompt, and launches a session whose input is that
/// rendered prompt (with the action's agent and branch). This is the GUI path for
/// "start a session pre-filled from a template".
fn launch_template_session_task(
    app: &PohunekApp,
    action_name: String,
) -> Result<Task<Message>, String> {
    let host = selected_host_config(app)?;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    let project = selected_project_reference(app)?;
    let terminal_size = terminal_size(app)?;
    Ok(Task::perform(
        runtime::perform(async move {
            let action = match resolve_project_action_with_options(
                &host,
                ProjectActionParams {
                    reference: project.clone(),
                    name: action_name,
                },
                options,
            )
            .await
            {
                Ok(action) => action,
                Err(err) => {
                    return Ok(CoreMessage::HostOperationFailed {
                        host_id,
                        error: err.to_string(),
                    })
                }
            };
            let preview = match preview_action_prompt(&action, String::new(), String::new()) {
                Ok(preview) => preview,
                Err(err) => {
                    return Ok(CoreMessage::HostOperationFailed {
                        host_id,
                        error: err.to_string(),
                    })
                }
            };
            match launch_action_prompt_with_options(
                &host,
                PromptLaunchParams {
                    project,
                    action,
                    preview,
                    cols: terminal_size.cols,
                    rows: terminal_size.rows,
                    metadata: BTreeMap::new(),
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

fn remove_project_task(app: &PohunekApp, prune_worktrees: bool) -> Result<Task<Message>, String> {
    let host = selected_host_config(app)?;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    let reference = selected_project_reference(app)?;
    let params = ProjectRemoveParams {
        reference: reference.clone(),
        prune_worktrees,
    };
    Ok(Task::perform(
        runtime::perform(async move {
            remove_project_with_options(&host, params, options)
                .await
                .map(|result| CoreMessage::ProjectRemoved {
                    host_id,
                    reference,
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
    let host = app
        .workspace
        .hosts
        .get(&host_id)
        .ok_or_else(|| format!("unknown host `{host_id}`"))?;
    let query = providers::linear::LinearQuery {
        state: optional_field(&host.provider.linear.state_filter),
        search: optional_field(&host.provider.linear.search),
        ..providers::linear::LinearQuery::default()
    };
    let state_filter = host.provider.linear.state_filter.clone();
    let search = host.provider.linear.search.clone();
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
            match client.assigned_issues(query).await {
                Ok(issues) => Ok(CoreMessage::LinearProviderIssuesLoaded {
                    host_id,
                    request_id,
                    state_filter,
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
    Ok(Task::perform(
        runtime::perform(async move {
            match client.list_pull_requests().await {
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

/// Builds session-creation params from the Start panel. The project and
/// terminal size come from the selected project and config; `cwd`/`repo` are
/// derived daemon-side from the project, so only the agent, optional initial
/// input and optional branch overrides are taken from the form.
fn build_session_params(
    form: &StartForm,
    project: String,
    terminal_size: TerminalSize,
) -> SessionNewParams {
    SessionNewParams {
        agent: form.agent.as_str().to_owned(),
        cwd: None,
        cols: terminal_size.cols,
        rows: terminal_size.rows,
        project: Some(project),
        repo: None,
        branch: optional_field(&form.branch),
        base_branch: optional_field(&form.base_branch),
        input: optional_field(&form.input),
        metadata: BTreeMap::new(),
    }
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

fn selected_host_config(app: &PohunekApp) -> Result<HostConfig, String> {
    let host_id = selected_host_id(app)?;
    host_config(app, &host_id)
}

fn selected_host_id(app: &PohunekApp) -> Result<HostId, String> {
    let host_id = match app.ui_state.selection.as_ref() {
        Some(
            Selection::Host { host_id }
            | Selection::Project { host_id, .. }
            | Selection::Session { host_id, .. },
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

    container(row![
        container(left).width(u32::from(app.ui_state.left_pane_width)),
        container(detail_view(app)).padding([0, 16]).width(Fill)
    ])
    .padding(16)
    .width(Fill)
    .height(Fill)
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
        tree = tree.push(
            row![
                caret(expanded, node),
                conn_dot(host.conn.clone()),
                text(host_id.to_string()).size(15)
            ]
            .spacing(6)
            .align_y(Center),
        );
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
    indent(
        2,
        row![
            status_dot(session.activity),
            list_button(
                text(format!(
                    "{}  {}{}",
                    session.id.0, session.agent, provider_status
                ))
                .size(14),
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
        list = list.push(
            row![
                status_dot(agent.activity),
                list_button(
                    text(format!(
                        "{} / {}  {}",
                        agent.host_id, agent.session_id.0, agent.agent
                    ))
                    .size(14),
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
        start_session_view(app),
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
            detail = detail
                .push(text(format!("{} / {}", host_id, session.id.0)).size(16))
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
                        .style(iced::widget::button::danger)
                ]
                .spacing(8),
            );
            detail = detail.push(metadata_view(app, session));
        }
        None => {
            detail = detail.push(text("No session selected").size(16));
        }
    }
    card(detail)
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
        if let Some(details) = app
            .workspace
            .hosts
            .get(host_id)
            .and_then(|host| host.project_details.get(&project.id))
        {
            detail = detail.push(text("Worktrees").size(16));
            for worktree in &details.worktrees {
                let branch = worktree.branch.as_deref().unwrap_or("detached");
                let owner = if worktree.owned { "owned" } else { "external" };
                let session = worktree.session_id.as_deref().unwrap_or("-");
                detail = detail.push(
                    text(format!(
                        "{}  branch={}  {}  session={}",
                        worktree.path.display(),
                        branch,
                        owner,
                        session
                    ))
                    .size(13),
                );
            }
        }
    } else {
        detail = detail.push(text("No project selected").size(16));
    }
    let detail = detail
        .push(
            button("Refresh")
                .on_press(Message::ShowProject)
                .style(iced::widget::button::secondary),
        )
        .push(text("Rename / remove").size(15))
        .push(
            row![
                text_input("new name", &app.project_edit.rename_to)
                    .on_input(Message::ProjectRenameToChanged),
                button("Rename")
                    .on_press(Message::RenameProject)
                    .style(iced::widget::button::secondary),
            ]
            .spacing(8),
        )
        .push(
            row![
                button("Remove")
                    .on_press(Message::RemoveProject {
                        prune_worktrees: false
                    })
                    .style(iced::widget::button::danger),
                button("Remove + prune")
                    .on_press(Message::RemoveProject {
                        prune_worktrees: true
                    })
                    .style(iced::widget::button::danger),
            ]
            .spacing(8),
        );
    card(detail)
}

/// Intent-driven "Start session" panel for the selected project. The operator
/// only picks the agent and an optional initial prompt; project, repo, cwd and
/// terminal size are derived. Branch/base-branch overrides hide behind Advanced.
fn start_session_view(app: &PohunekApp) -> Element<'_, Message> {
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
    let mut panel = column![
        section_title("Start a session"),
        row![
            text("Agent").size(14),
            pick_list(
                AgentChoice::ALL,
                Some(app.start.agent),
                Message::StartAgentSelected
            ),
        ]
        .spacing(8)
        .align_y(Center),
        row![
            text("Template").size(14),
            pick_list(
                template_options,
                template_selected,
                Message::StartTemplateSelected
            ),
        ]
        .spacing(8)
        .align_y(Center),
    ]
    .spacing(8);
    // Free input is only used for a blank session; a template supplies its own.
    if app.start.template.is_none() {
        panel = panel.push(
            text_input("initial input (optional)", &app.start.input)
                .on_input(Message::StartInputChanged),
        );
    } else {
        panel = panel.push(text("Input comes from the selected template's prompt.").size(13));
    }
    panel = panel.push(
        button(text(advanced_label).size(13))
            .on_press(Message::ToggleStartAdvanced)
            .style(iced::widget::button::text),
    );
    if app.start.show_advanced {
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
    card(panel)
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
            .on_press(Message::Core(CoreMessage::ProviderPanelSelected {
                host_id: host_id.clone(),
                panel: ProviderPanel::Linear,
            }))
            .style(tab_style(ProviderPanel::Linear)),
        button("GitHub")
            .on_press(Message::Core(CoreMessage::ProviderPanelSelected {
                host_id: host_id.clone(),
                panel: ProviderPanel::GitHub,
            }))
            .style(tab_style(ProviderPanel::GitHub))
    ]
    .spacing(8);
    let current_scope = selected_github_scope(app).ok();
    let selected_action = app.selected_action.clone();
    let body = match active {
        ProviderPanel::Linear => linear_provider_view(
            host_id.clone(),
            host,
            available_actions(app, &ProviderKind::LinearIssue),
            selected_action,
        ),
        ProviderPanel::GitHub => github_provider_view(
            host_id.clone(),
            current_scope,
            host,
            available_actions(app, &ProviderKind::GithubPr),
            selected_action,
        ),
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
    let selected = selected_action.filter(|name| actions.contains(name));
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

#[expect(
    clippy::needless_pass_by_value,
    reason = "owning host_id keeps the returned Iced element lifetime tied only to host state"
)]
fn linear_provider_view(
    host_id: HostId,
    host: &pohunek_gui_core::HostView,
    actions: Vec<String>,
    selected_action: Option<String>,
) -> Element<'_, Message> {
    let state = &host.provider.linear;
    let mut view = column![row![
        text_input("state", &state.state_filter).on_input({
            let host_id = host_id.clone();
            move |value| {
                Message::Core(CoreMessage::LinearProviderStateFilterChanged {
                    host_id: host_id.clone(),
                    value,
                })
            }
        }),
        text_input("search", &state.search).on_input({
            let host_id = host_id.clone();
            move |value| {
                Message::Core(CoreMessage::LinearProviderSearchChanged {
                    host_id: host_id.clone(),
                    value,
                })
            }
        }),
        button("Fetch assigned")
            .on_press(Message::FetchLinearIssues)
            .style(iced::widget::button::secondary),
    ]
    .spacing(8)]
    .spacing(8);
    if !state.issues.is_empty() {
        view = view.push(text("Pick an issue, choose an action, then Launch.").size(12));
    }
    for issue in &state.issues {
        let issue_id = issue.prompt_item_id().to_owned();
        let selected = state.selected_issue_id.as_deref() == Some(issue_id.as_str());
        view = view.push(list_button(
            text(format!("{}  {}", issue.prompt_item_id(), issue.title)).size(13),
            Message::Core(CoreMessage::LinearProviderIssueSelected {
                host_id: host_id.clone(),
                issue_id,
            }),
            selected,
        ));
    }
    if let Some(issue) = selected_linear_issue_in_state(state) {
        view = view
            .push(text(format!("{}  {}", issue.prompt_item_id(), issue.url)).size(13))
            .push(text(issue.body.clone()).size(13))
            .push(action_launcher(
                actions,
                selected_action,
                Message::LaunchLinearIssue,
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
    actions: Vec<String>,
    selected_action: Option<String>,
) -> Element<'_, Message> {
    let state = &host.provider.github;
    let mut view = column![row![
        text_input("search", &state.search).on_input({
            let host_id = host_id.clone();
            move |value| {
                Message::Core(CoreMessage::GitHubProviderSearchChanged {
                    host_id: host_id.clone(),
                    value,
                })
            }
        }),
        button("Fetch PRs")
            .on_press(Message::FetchGitHubPullRequests)
            .style(iced::widget::button::secondary),
        button("Fetch issues")
            .on_press(Message::FetchGitHubIssues)
            .style(iced::widget::button::secondary),
        button("Refresh PR status")
            .on_press(Message::FetchGitHubPullRequestStatus)
            .style(iced::widget::button::secondary),
    ]
    .spacing(8)]
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
    view = view.push(text("Pick a pull request or issue, choose an action, then Launch.").size(12));
    view = view.push(text("Pull requests").size(15));
    for pull_request in filtered_pull_requests(state) {
        let selected = state.selected_pull_request == Some(pull_request.number);
        view = view.push(list_button(
            text(format!("#{}  {}", pull_request.number, pull_request.title)).size(13),
            Message::Core(CoreMessage::GitHubProviderPullRequestSelected {
                host_id: host_id.clone(),
                number: pull_request.number,
            }),
            selected,
        ));
    }
    if let Some(pull_request) = selected_pull_request_in_state(state) {
        let status_key = state
            .scope
            .clone()
            .map(|scope| GitHubPullRequestStatusKey::new(scope, pull_request.url.clone()));
        let status = status_key
            .as_ref()
            .and_then(|key| state.pull_request_statuses.get(key))
            .map_or_else(|| "status unknown".to_owned(), format_pr_status);
        view = view
            .push(
                text(format!(
                    "{}  {}",
                    pull_request.head_ref_name, pull_request.url
                ))
                .size(13),
            )
            .push(text(format!("status: {status}")).size(13))
            .push(text(pull_request.body.clone()).size(13))
            .push(action_launcher(
                actions,
                selected_action,
                Message::LaunchGitHubPullRequest,
            ));
    }
    view = view.push(text("Issues").size(15));
    for issue in filtered_github_issues(state) {
        let selected = state.selected_issue == Some(issue.number);
        view = view.push(list_button(
            text(format!("#{}  {}", issue.number, issue.title)).size(13),
            Message::Core(CoreMessage::GitHubProviderIssueSelected {
                host_id: host_id.clone(),
                number: issue.number,
            }),
            selected,
        ));
    }
    if let Some(issue) = selected_github_issue_in_state(state) {
        view = view
            .push(text(format!("#{}  {}", issue.number, issue.url)).size(13))
            .push(text(issue.body.clone()).size(13));
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
    let review = match &status.review_decision {
        providers::github::ReviewDecision::Approved => "approved",
        providers::github::ReviewDecision::ChangesRequested => "changes requested",
        providers::github::ReviewDecision::ReviewRequired => "review required",
        providers::github::ReviewDecision::None => "no review",
        providers::github::ReviewDecision::Unknown(value) => value.as_str(),
    };
    let mut pass = 0;
    let mut fail = 0;
    let mut pending = 0;
    for check in &status.checks {
        match check.conclusion.as_deref().unwrap_or(check.status.as_str()) {
            "pass" | "SUCCESS" | "COMPLETED" => pass += 1,
            "fail" | "FAILURE" | "ERROR" | "cancel" => fail += 1,
            _ => pending += 1,
        }
    }
    format!("review={review} checks={pass} pass/{fail} fail/{pending} pending")
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
}

#[derive(Debug, Deserialize)]
struct RawGitHubProviderConfig {
    gh_bin: PathBuf,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

impl RawProvidersConfig {
    fn into_provider_config(self) -> Result<ProviderAppConfig, ConfigError> {
        Ok(ProviderAppConfig {
            linear: self
                .linear
                .map(RawLinearProviderConfig::into_app_config)
                .transpose()?,
            github: self
                .github
                .map(RawGitHubProviderConfig::into_app_config)
                .transpose()?,
        })
    }
}

impl RawLinearProviderConfig {
    fn into_app_config(self) -> Result<LinearAppConfig, ConfigError> {
        Ok(LinearAppConfig {
            token_key: non_empty_config_value(self.token_key, "providers.linear.token_key")?,
            endpoint: validate_http_endpoint(self.endpoint, "providers.linear.endpoint")?,
            token_lookup_timeout: required_duration_millis(
                self.token_timeout_ms,
                "providers.linear.token_timeout_ms",
            )?,
        })
    }
}

impl RawGitHubProviderConfig {
    fn into_app_config(self) -> Result<GitHubAppConfig, ConfigError> {
        Ok(GitHubAppConfig {
            gh_bin: non_empty_config_path(self.gh_bin, "providers.github.gh_bin")?,
            timeout: duration_millis(
                self.timeout_ms,
                "providers.github.timeout_ms",
                Duration::from_secs(20),
            )?,
        })
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
    Ok(PathBuf::from(require_env("XDG_RUNTIME_DIR")?)
        .join("pohunek")
        .join("daemon.sock"))
}

fn config_dir() -> Result<PathBuf, ConfigError> {
    if let Ok(value) = std::env::var("XDG_CONFIG_HOME") {
        if !value.is_empty() {
            return Ok(PathBuf::from(value).join("pohunek"));
        }
    }
    Ok(PathBuf::from(require_env("HOME")?)
        .join(".config")
        .join("pohunek"))
}

fn require_env(key: &'static str) -> Result<String, ConfigError> {
    match std::env::var(key) {
        Ok(value) if !value.is_empty() => Ok(value),
        _ => Err(ConfigError::MissingEnv {
            var: key.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            activity_filter: None,
            metadata_edit: MetadataEdit::default(),
            project_edit: ProjectEdit {
                reference: "manual-project".to_owned(),
                ..ProjectEdit::default()
            },
            selected_action: None,
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
    fn provider_config_rejects_zero_linear_token_timeout() {
        let err = RawLinearProviderConfig {
            token_key: "linear-token-ref".to_owned(),
            endpoint: "https://linear.example/graphql".to_owned(),
            token_timeout_ms: 0,
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
