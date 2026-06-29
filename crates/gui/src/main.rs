//! Native Iced shell for the pohunek control plane.

// Rust guideline compliant 2026-06-26
#![forbid(unsafe_code)]

mod runtime;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use iced::widget::{button, column, container, row, scrollable, text, text_input};
use iced::{window, Element, Fill, Size, Subscription, Task, Theme};
use pohunek_gui_core::{
    add_project_with_options, create_session_with_options, default_state_dir, discover_hosts,
    inspect_session_with_options, launch_action_prompt_with_options,
    list_project_actions_with_options, list_projects_with_options, preview_action_prompt,
    preview_prompt_content, remove_project_with_options, rename_project_with_options,
    resolve_project_action_with_options, resolve_project_prompt_with_options,
    session_metadata_rows, set_session_metadata_with_options, show_project_with_options,
    spawn_attach_command, stop_session_with_options, AttachCommandSpawner, AttachTemplateValues,
    ConnState, ConnectionOptions, HostConfig, HostId, Message as CoreMessage, NotificationIntent,
    PromptContext, PromptLaunchParams, PromptProvider, Selection, Toast, TreeNodeId, UiState,
    WindowSize, Workspace,
};
use protocol::{
    ProjectActionParams, ProjectActionsParams, ProjectAddParams, ProjectInfo, ProjectPromptParams,
    ProjectRemoveParams, ProjectRenameParams, ProjectShowParams, ProviderKind, SessionId,
    SessionInfo, SessionNewParams, SessionSetMetadataParams,
};
use serde::Deserialize;
use thiserror::Error;

const DEFAULT_NOTIFICATION_COMMAND: &str = "notify-send";

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
    new_session: NewSessionForm,
    metadata_edit: MetadataEdit,
    project_edit: ProjectEdit,
    prompt_edit: PromptEdit,
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
                new_session: NewSessionForm::default(),
                metadata_edit: MetadataEdit::default(),
                project_edit: ProjectEdit::default(),
                prompt_edit: PromptEdit::default(),
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

#[derive(Debug, Clone)]
struct NewSessionForm {
    agent: String,
    cwd: String,
    project: String,
    repo: String,
    branch: String,
    base_branch: String,
    input: String,
    metadata_key: String,
    metadata_value: String,
    cols: String,
    rows: String,
}

impl Default for NewSessionForm {
    fn default() -> Self {
        Self {
            agent: "codex".to_owned(),
            cwd: String::new(),
            project: String::new(),
            repo: String::new(),
            branch: String::new(),
            base_branch: String::new(),
            input: String::new(),
            metadata_key: String::new(),
            metadata_value: String::new(),
            cols: "80".to_owned(),
            rows: "24".to_owned(),
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
struct PromptEdit {
    reference: String,
    prompt_name: String,
    action_name: String,
    provider: String,
    item_id: String,
    context_json: String,
}

impl Default for PromptEdit {
    fn default() -> Self {
        Self {
            reference: String::new(),
            prompt_name: "issue".to_owned(),
            action_name: "process-issue".to_owned(),
            provider: "linear_issue".to_owned(),
            item_id: String::new(),
            context_json: "{}".to_owned(),
        }
    }
}

#[derive(Debug, Clone)]
enum Message {
    Core(CoreMessage),
    HostsDiscovered(DiscoveryResult),
    ToggleNode(TreeNodeId),
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
    NewSessionAgentChanged(String),
    NewSessionCwdChanged(String),
    NewSessionProjectChanged(String),
    NewSessionRepoChanged(String),
    NewSessionBranchChanged(String),
    NewSessionBaseBranchChanged(String),
    NewSessionInputChanged(String),
    NewSessionMetadataKeyChanged(String),
    NewSessionMetadataValueChanged(String),
    NewSessionColsChanged(String),
    NewSessionRowsChanged(String),
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
    ProjectReferenceChanged(String),
    ProjectRenameToChanged(String),
    ListProjects,
    AddProject,
    ShowProject,
    RenameProject,
    RemoveProject {
        prune_worktrees: bool,
    },
    PromptReferenceChanged(String),
    PromptNameChanged(String),
    PromptActionChanged(String),
    PromptProviderChanged(String),
    PromptItemIdChanged(String),
    PromptContextChanged(String),
    ListProjectActions,
    ResolveProjectPrompt,
    ResolveProjectAction,
    PreviewPrompt,
    PreviewAction,
    LaunchPromptAction,
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
            app.workspace.selection = Some(Selection::Project {
                host_id: host_id.clone(),
                project_id: project_id.clone(),
            });
            app.ui_state.selection = app.workspace.selection.clone();
            app.project_edit.reference = project_id;
            app.prompt_edit.reference = app.project_edit.reference.clone();
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
        Message::NewSessionAgentChanged(value) => app.new_session.agent = value,
        Message::NewSessionCwdChanged(value) => app.new_session.cwd = value,
        Message::NewSessionProjectChanged(value) => app.new_session.project = value,
        Message::NewSessionRepoChanged(value) => app.new_session.repo = value,
        Message::NewSessionBranchChanged(value) => app.new_session.branch = value,
        Message::NewSessionBaseBranchChanged(value) => app.new_session.base_branch = value,
        Message::NewSessionInputChanged(value) => app.new_session.input = value,
        Message::NewSessionMetadataKeyChanged(value) => app.new_session.metadata_key = value,
        Message::NewSessionMetadataValueChanged(value) => app.new_session.metadata_value = value,
        Message::NewSessionColsChanged(value) => app.new_session.cols = value,
        Message::NewSessionRowsChanged(value) => app.new_session.rows = value,
        Message::CreateSession => match create_session_task(app) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
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
        Message::ProjectReferenceChanged(value) => app.project_edit.reference = value,
        Message::ProjectRenameToChanged(value) => app.project_edit.rename_to = value,
        Message::ListProjects => match list_projects_task(app) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
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
        Message::PromptReferenceChanged(value) => app.prompt_edit.reference = value,
        Message::PromptNameChanged(value) => app.prompt_edit.prompt_name = value,
        Message::PromptActionChanged(value) => app.prompt_edit.action_name = value,
        Message::PromptProviderChanged(value) => app.prompt_edit.provider = value,
        Message::PromptItemIdChanged(value) => app.prompt_edit.item_id = value,
        Message::PromptContextChanged(value) => app.prompt_edit.context_json = value,
        Message::ListProjectActions => match list_project_actions_task(app) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::ResolveProjectPrompt => match resolve_project_prompt_task(app) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::ResolveProjectAction => match resolve_project_action_task(app) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::PreviewPrompt => match preview_prompt_message(app) {
            Ok(message) => app.workspace.apply(message),
            Err(err) => app.status = Some(err),
        },
        Message::PreviewAction => match preview_action_message(app) {
            Ok(message) => app.workspace.apply(message),
            Err(err) => app.status = Some(err),
        },
        Message::LaunchPromptAction => match launch_prompt_action_task(app) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
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
    let params = build_session_params(&app.new_session)?;
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

fn list_projects_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let host = selected_host_config(app)?;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    Ok(Task::perform(
        runtime::perform(async move {
            list_projects_with_options(&host, options)
                .await
                .map(|projects| CoreMessage::ProjectListLoaded { host_id, projects })
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
    let reference = selected_prompt_project_reference(app)?;
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

fn resolve_project_prompt_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let host = selected_host_config(app)?;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    let params = ProjectPromptParams {
        reference: selected_prompt_project_reference(app)?,
        name: required_field(&app.prompt_edit.prompt_name, "prompt name")?,
    };
    Ok(Task::perform(
        runtime::perform(async move {
            match resolve_project_prompt_with_options(&host, params, options).await {
                Ok(prompt) => Ok(CoreMessage::ProjectPromptResolved { host_id, prompt }),
                Err(err) => Ok(CoreMessage::HostOperationFailed {
                    host_id,
                    error: err.to_string(),
                }),
            }
        }),
        Message::CoreCommandCompleted,
    ))
}

fn resolve_project_action_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let host = selected_host_config(app)?;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    let params = ProjectActionParams {
        reference: selected_prompt_project_reference(app)?,
        name: required_field(&app.prompt_edit.action_name, "action name")?,
    };
    Ok(Task::perform(
        runtime::perform(async move {
            match resolve_project_action_with_options(&host, params, options).await {
                Ok(action) => Ok(CoreMessage::ProjectActionResolved { host_id, action }),
                Err(err) => Ok(CoreMessage::HostOperationFailed {
                    host_id,
                    error: err.to_string(),
                }),
            }
        }),
        Message::CoreCommandCompleted,
    ))
}

fn preview_prompt_message(app: &PohunekApp) -> Result<CoreMessage, String> {
    let host_id = selected_host_id(app)?;
    let host = app
        .workspace
        .hosts
        .get(&host_id)
        .ok_or_else(|| format!("unknown host `{host_id}`"))?;
    let prompt = host
        .prompt
        .resolved_prompt
        .as_ref()
        .ok_or_else(|| "resolve a prompt first".to_owned())?;
    match preview_prompt_content(
        prompt.name.clone(),
        &prompt.content,
        &PromptContext {
            provider: parse_prompt_provider(&app.prompt_edit.provider)?,
            item_id: required_field(&app.prompt_edit.item_id, "item id")?,
            json: required_field(&app.prompt_edit.context_json, "context JSON")?,
        },
    ) {
        Ok(preview) => Ok(CoreMessage::PromptPreviewRendered { host_id, preview }),
        Err(err) => Ok(CoreMessage::HostOperationFailed {
            host_id,
            error: err.to_string(),
        }),
    }
}

fn preview_action_message(app: &PohunekApp) -> Result<CoreMessage, String> {
    let host_id = selected_host_id(app)?;
    let host = app
        .workspace
        .hosts
        .get(&host_id)
        .ok_or_else(|| format!("unknown host `{host_id}`"))?;
    let action = host
        .prompt
        .resolved_action
        .as_ref()
        .ok_or_else(|| "resolve an action first".to_owned())?;
    let (item_id, context_json) = if action.provider == ProviderKind::None {
        (String::new(), String::new())
    } else {
        (
            required_field(&app.prompt_edit.item_id, "item id")?,
            required_field(&app.prompt_edit.context_json, "context JSON")?,
        )
    };
    match preview_action_prompt(action, item_id, context_json) {
        Ok(preview) => Ok(CoreMessage::PromptPreviewRendered { host_id, preview }),
        Err(err) => Ok(CoreMessage::HostOperationFailed {
            host_id,
            error: err.to_string(),
        }),
    }
}

fn launch_prompt_action_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let host = selected_host_config(app)?;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    let host_view = app
        .workspace
        .hosts
        .get(&host_id)
        .ok_or_else(|| format!("unknown host `{host_id}`"))?;
    let action = host_view
        .prompt
        .resolved_action
        .clone()
        .ok_or_else(|| "resolve an action first".to_owned())?;
    let preview = host_view
        .prompt
        .preview
        .clone()
        .ok_or_else(|| "preview a rendered action first".to_owned())?;
    if preview.prompt_name != action.prompt_name {
        return Err("preview the resolved action before launching it".to_owned());
    }
    let project = selected_prompt_project_reference(app)?;
    Ok(Task::perform(
        runtime::perform(async move {
            match launch_action_prompt_with_options(
                &host,
                PromptLaunchParams {
                    project,
                    action,
                    preview,
                    cols: 80,
                    rows: 24,
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

fn build_session_params(form: &NewSessionForm) -> Result<SessionNewParams, String> {
    let metadata = optional_metadata(&form.metadata_key, &form.metadata_value)?;
    Ok(SessionNewParams {
        agent: required_field(&form.agent, "agent")?,
        cwd: optional_field(&form.cwd).map(PathBuf::from),
        cols: parse_u16(&form.cols, "columns")?,
        rows: parse_u16(&form.rows, "rows")?,
        project: optional_field(&form.project),
        repo: optional_field(&form.repo).map(PathBuf::from),
        branch: optional_field(&form.branch),
        base_branch: optional_field(&form.base_branch),
        input: optional_field(&form.input),
        metadata,
    })
}

fn optional_metadata(key: &str, value: &str) -> Result<BTreeMap<String, String>, String> {
    let Some(key) = optional_field(key) else {
        return Ok(BTreeMap::new());
    };
    if value.is_empty() {
        return Err("metadata value must not be empty when metadata key is set".to_owned());
    }
    Ok(BTreeMap::from([(key, value.to_owned())]))
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

fn selected_prompt_project_reference(app: &PohunekApp) -> Result<String, String> {
    optional_field(&app.prompt_edit.reference)
        .or_else(|| selected_project_reference(app).ok())
        .ok_or_else(|| "select or enter a project reference".to_owned())
}

fn parse_prompt_provider(value: &str) -> Result<PromptProvider, String> {
    value
        .parse::<PromptProvider>()
        .map_err(|err| err.to_string())
}

fn connection_options(app: &PohunekApp) -> Result<ConnectionOptions, String> {
    app.config
        .as_ref()
        .map(|config| config.connection_options)
        .map_err(Clone::clone)
}

fn optional_field(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn required_field(value: &str, label: &str) -> Result<String, String> {
    optional_field(value).ok_or_else(|| format!("{label} is required"))
}

fn parse_u16(value: &str, label: &str) -> Result<u16, String> {
    let parsed = required_field(value, label)?
        .parse::<u16>()
        .map_err(|err| format!("invalid {label}: {err}"))?;
    if parsed == 0 {
        Err(format!("{label} must be greater than zero"))
    } else {
        Ok(parsed)
    }
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

fn view(app: &PohunekApp) -> Element<'_, Message> {
    let left = column![
        container(workspace_tree(app)).height(Fill),
        container(agents_monitor(app)).height(u32::from(app.ui_state.agents_pane_height))
    ]
    .spacing(12);

    container(row![
        container(left).width(u32::from(app.ui_state.left_pane_width)),
        container(detail_view(app)).width(Fill)
    ])
    .padding(16)
    .width(Fill)
    .height(Fill)
    .into()
}

fn workspace_tree(app: &PohunekApp) -> Element<'_, Message> {
    let mut tree = column![text("Workspace").size(20)].spacing(6);
    if let Err(err) = &app.config {
        tree = tree.push(text(format!("configuration error: {err}")).size(14));
        return scrollable(tree).into();
    }
    for (host_id, host) in &app.workspace.hosts {
        let node = TreeNodeId::host(host_id.clone());
        let expanded = app.ui_state.expanded_nodes.contains(&node);
        tree = tree.push(row![
            button(if expanded { "v" } else { ">" }).on_press(Message::ToggleNode(node)),
            text(format!("{} [{}]", host_id, conn_label(&host.conn))).size(16)
        ]);
        if let Some(error) = &host.last_error {
            tree = tree.push(text(format!("  {error}")).size(13));
        }
        if expanded {
            tree = push_project_rows(tree, app, host_id, host);
        }
    }
    if app.workspace.hosts.is_empty() {
        tree = tree.push(text("connecting").size(14));
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
    tree = tree.push(row![
        text("  "),
        button(if expanded { "v" } else { ">" }).on_press(Message::ToggleNode(node)),
        button(text(format!("Unknown project {project_id}")).size(15)).on_press(
            Message::SelectProject {
                host_id: host_id.clone(),
                project_id: project_id.to_owned(),
            },
        )
    ]);
    if expanded {
        for session in host
            .sessions
            .values()
            .filter(|session| session.project_id.as_deref() == Some(project_id))
        {
            tree = tree.push(session_tree_row(host_id, session));
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
    tree = tree.push(row![
        text("  "),
        button(if expanded { "v" } else { ">" }).on_press(Message::ToggleNode(node)),
        button(text(label).size(15)).on_press(Message::SelectProject {
            host_id: host_id.clone(),
            project_id,
        })
    ]);
    if expanded {
        for session in host.sessions.values().filter(|session| {
            project.map_or_else(
                || session.project_id.is_none(),
                |project| session.project_id.as_deref() == Some(project.id.as_str()),
            )
        }) {
            tree = tree.push(session_tree_row(host_id, session));
        }
    }
    tree
}

fn session_tree_row(host_id: &HostId, session: &SessionInfo) -> Element<'static, Message> {
    let activity = session
        .activity
        .map_or("unknown", |activity| activity.as_str());
    row![
        text("    "),
        button(text(format!(
            "{}  {}  [{}]",
            session.id.0, session.agent, activity
        )))
        .on_press(Message::SelectSession {
            host_id: host_id.clone(),
            session_id: session.id.clone(),
        })
    ]
    .into()
}

fn agents_monitor(app: &PohunekApp) -> Element<'_, Message> {
    let monitor = app.workspace.agent_monitor();
    let mut list = column![
        text("Agents").size(18),
        text(format!(
            "blocked {}  working {}  idle {}  unknown {}",
            monitor.blocked, monitor.working, monitor.idle, monitor.unknown
        ))
        .size(13)
    ]
    .spacing(5);
    for row in monitor.sessions {
        let activity = row.activity.map_or("unknown", |activity| activity.as_str());
        list = list.push(
            button(text(format!(
                "{}  {} / {}  {}",
                activity, row.host_id, row.session_id.0, row.agent
            )))
            .on_press(Message::SelectSession {
                host_id: row.host_id,
                session_id: row.session_id,
            }),
        );
    }
    scrollable(list).into()
}

fn detail_view(app: &PohunekApp) -> Element<'_, Message> {
    let mut detail = column![text("Workspace detail").size(22)].spacing(12);
    match app.ui_state.selection.as_ref() {
        Some(Selection::Session { .. }) => {
            detail = detail.push(session_detail(app));
        }
        Some(Selection::Project { .. }) => {
            detail = detail.push(project_detail(app));
        }
        Some(Selection::Host { host_id }) => {
            detail = detail.push(text(format!("host: {host_id}")).size(16));
        }
        None => {
            detail = detail.push(text("No session or project selected").size(16));
        }
    }
    detail = detail
        .push(new_session_view(app))
        .push(project_management_view(app))
        .push(prompt_management_view(app));
    for toast in app.workspace.toasts.iter().rev().take(3).rev() {
        detail = detail.push(toast_view(toast));
    }
    if let Some(status) = &app.status {
        detail = detail.push(text(status).size(13));
    }
    scrollable(detail).into()
}

fn session_detail(app: &PohunekApp) -> Element<'_, Message> {
    let mut detail = column![text("Session").size(18)].spacing(8);
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
            if let Some(path) = &session.worktree_path {
                detail = detail.push(text(format!("worktree: {}", path.display())).size(14));
            }
            detail = detail.push(text(format!("cwd: {}", session.cwd.display())).size(14));
            detail = detail.push(row![
                button("Open in terminal").on_press(Message::OpenSession {
                    host_id: host_id.clone(),
                    session_id: session.id.clone(),
                }),
                button("Inspect").on_press(Message::InspectSelectedSession),
                button("Stop").on_press(Message::StopSelectedSession)
            ]);
            detail = detail.push(metadata_view(app, session));
        }
        None => {
            detail = detail.push(text("No session selected").size(16));
        }
    }
    detail.into()
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
        .push(row![
            button("Set metadata").on_press(Message::SetMetadata),
            button("Clear key").on_press(Message::ClearMetadata)
        ]);
    metadata.into()
}

fn project_detail(app: &PohunekApp) -> Element<'_, Message> {
    let mut detail = column![text("Project").size(18)].spacing(8);
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
    detail
        .push(button("Show project").on_press(Message::ShowProject))
        .into()
}

fn new_session_view(app: &PohunekApp) -> Element<'_, Message> {
    column![
        text("New session").size(18),
        row![
            text_input("agent", &app.new_session.agent).on_input(Message::NewSessionAgentChanged),
            text_input("cols", &app.new_session.cols).on_input(Message::NewSessionColsChanged),
            text_input("rows", &app.new_session.rows).on_input(Message::NewSessionRowsChanged)
        ]
        .spacing(8),
        row![
            text_input("cwd", &app.new_session.cwd).on_input(Message::NewSessionCwdChanged),
            text_input("project", &app.new_session.project)
                .on_input(Message::NewSessionProjectChanged),
            text_input("repo", &app.new_session.repo).on_input(Message::NewSessionRepoChanged)
        ]
        .spacing(8),
        row![
            text_input("branch", &app.new_session.branch)
                .on_input(Message::NewSessionBranchChanged),
            text_input("base branch", &app.new_session.base_branch)
                .on_input(Message::NewSessionBaseBranchChanged)
        ]
        .spacing(8),
        text_input("initial input", &app.new_session.input)
            .on_input(Message::NewSessionInputChanged),
        row![
            text_input("metadata key", &app.new_session.metadata_key)
                .on_input(Message::NewSessionMetadataKeyChanged),
            text_input("metadata value", &app.new_session.metadata_value)
                .on_input(Message::NewSessionMetadataValueChanged)
        ]
        .spacing(8),
        button("Create session").on_press(Message::CreateSession)
    ]
    .spacing(8)
    .into()
}

fn project_management_view(app: &PohunekApp) -> Element<'_, Message> {
    column![
        text("Projects").size(18),
        row![
            text_input("path", &app.project_edit.path).on_input(Message::ProjectPathChanged),
            text_input("name", &app.project_edit.name).on_input(Message::ProjectNameChanged),
            text_input("base branch", &app.project_edit.base_branch)
                .on_input(Message::ProjectBaseBranchChanged),
            button("Add").on_press(Message::AddProject)
        ]
        .spacing(8),
        row![
            text_input("reference", &app.project_edit.reference)
                .on_input(Message::ProjectReferenceChanged),
            text_input("rename to", &app.project_edit.rename_to)
                .on_input(Message::ProjectRenameToChanged)
        ]
        .spacing(8),
        row![
            button("List").on_press(Message::ListProjects),
            button("Show").on_press(Message::ShowProject),
            button("Rename").on_press(Message::RenameProject),
            button("Remove").on_press(Message::RemoveProject {
                prune_worktrees: false
            }),
            button("Remove + prune").on_press(Message::RemoveProject {
                prune_worktrees: true
            })
        ]
        .spacing(8)
    ]
    .spacing(8)
    .into()
}

fn prompt_management_view(app: &PohunekApp) -> Element<'_, Message> {
    let mut view = column![
        text("Prompts").size(18),
        row![
            text_input("project reference", &app.prompt_edit.reference)
                .on_input(Message::PromptReferenceChanged),
            button("List actions").on_press(Message::ListProjectActions)
        ]
        .spacing(8),
        row![
            text_input("action", &app.prompt_edit.action_name)
                .on_input(Message::PromptActionChanged),
            button("Resolve action").on_press(Message::ResolveProjectAction),
            button("Preview action").on_press(Message::PreviewAction),
            button("Launch").on_press(Message::LaunchPromptAction)
        ]
        .spacing(8),
        row![
            text_input("prompt", &app.prompt_edit.prompt_name).on_input(Message::PromptNameChanged),
            button("Resolve prompt").on_press(Message::ResolveProjectPrompt),
            button("Preview prompt").on_press(Message::PreviewPrompt)
        ]
        .spacing(8),
        row![
            text_input("provider", &app.prompt_edit.provider)
                .on_input(Message::PromptProviderChanged),
            text_input("item id", &app.prompt_edit.item_id).on_input(Message::PromptItemIdChanged)
        ]
        .spacing(8),
        text_input("provider context JSON", &app.prompt_edit.context_json)
            .on_input(Message::PromptContextChanged),
    ]
    .spacing(8);

    if let Some(host) = selected_prompt_host(app) {
        view = push_prompt_state(view, host);
    }
    view.into()
}

fn selected_prompt_host(app: &PohunekApp) -> Option<&pohunek_gui_core::HostView> {
    let host_id = selected_host_id(app).ok()?;
    app.workspace.hosts.get(&host_id)
}

fn push_prompt_state<'a>(
    mut view: iced::widget::Column<'a, Message>,
    host: &'a pohunek_gui_core::HostView,
) -> iced::widget::Column<'a, Message> {
    for (reference, actions) in &host.prompt.actions_by_project {
        view = view.push(text(format!("actions for {reference}")).size(15));
        for action in &actions.actions {
            view = view.push(
                text(format!(
                    "{}  provider={}  template={}  layer={}",
                    action.name,
                    provider_kind_label(&action.provider),
                    action.template,
                    prompt_layer_label(action.layer)
                ))
                .size(13),
            );
        }
    }
    if let Some(prompt) = &host.prompt.resolved_prompt {
        view = view.push(
            text(format!(
                "prompt {} resolved from {}",
                prompt.name,
                prompt_layer_label(prompt.layer)
            ))
            .size(13),
        );
    }
    if let Some(action) = &host.prompt.resolved_action {
        view = view.push(
            text(format!(
                "action recipe: agent={} provider={} prompt={}",
                action.agent,
                provider_kind_label(&action.provider),
                action.prompt_name
            ))
            .size(13),
        );
    }
    if let Some(preview) = &host.prompt.preview {
        view = view
            .push(text(format!("preview: {}", preview.prompt_name)).size(15))
            .push(text(preview.rendered.clone()).size(13));
    }
    view
}

fn provider_kind_label(provider: &ProviderKind) -> &'static str {
    match provider {
        ProviderKind::LinearIssue => "linear_issue",
        ProviderKind::GithubPr => "github_pr",
        ProviderKind::None => "none",
    }
}

fn prompt_layer_label(layer: protocol::PromptLayer) -> &'static str {
    match layer {
        protocol::PromptLayer::InRepo => "in-repo",
        protocol::PromptLayer::Host => "host",
    }
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
    notification_command: String,
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
        let connection_options = raw.gui.unwrap_or_default().connection_options()?;
        Ok(Self {
            attach_command: raw.attach_command,
            pohunek_bin: raw.pohunek_bin,
            local_host,
            connection_options,
            notification_command: raw
                .notification_command
                .unwrap_or_else(|| DEFAULT_NOTIFICATION_COMMAND.to_owned()),
        })
    }
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    attach_command: String,
    pohunek_bin: String,
    #[serde(default)]
    notification_command: Option<String>,
    #[serde(default)]
    gui: Option<RawGuiConfig>,
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
}

impl RawGuiConfig {
    fn connection_options(self) -> Result<ConnectionOptions, ConfigError> {
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
