//! Native Iced shell for the pohunek control plane.

// Rust guideline compliant 2026-06-26
#![forbid(unsafe_code)]

mod runtime;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use iced::widget::{button, column, container, row, scrollable, text};
use iced::{window, Element, Fill, Size, Subscription, Task, Theme};
use pohunek_gui_core::{
    default_state_dir, discover_hosts, render_attach_command, AttachTemplateValues, ConnState,
    ConnectionOptions, HostConfig, HostId, Message as CoreMessage, NotificationIntent, Selection,
    Toast, TreeNodeId, UiState, WindowSize, Workspace,
};
use protocol::{ProjectInfo, SessionId, SessionInfo};
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
                state_dir: boot.state_dir,
                status: boot.status,
                notified_intents: 0,
            },
            task,
        )
    }

    fn attach_command(&self, host_id: &HostId, session_id: &SessionId) -> Result<String, String> {
        let config = self.config.as_ref().map_err(Clone::clone)?;
        let host = self
            .hosts
            .iter()
            .find(|host| &host.id == host_id)
            .ok_or_else(|| format!("unknown host `{host_id}`"))?;
        Ok(render_attach_command(
            &config.attach_command,
            &AttachTemplateValues {
                bin: config.pohunek_bin.clone(),
                host: host.attach_host().to_owned(),
                id: session_id.0.clone(),
            },
        ))
    }
}

#[derive(Debug, Clone)]
#[expect(
    clippy::large_enum_variant,
    reason = "Iced messages carry core protocol messages directly from subscriptions"
)]
enum Message {
    Core(CoreMessage),
    HostsDiscovered(DiscoveryResult),
    ToggleNode(TreeNodeId),
    SelectSession {
        host_id: HostId,
        session_id: SessionId,
    },
    OpenSession {
        host_id: HostId,
        session_id: SessionId,
    },
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
        Message::OpenSession {
            host_id,
            session_id,
        } => match app.attach_command(&host_id, &session_id) {
            Ok(command) => tasks.push(Task::perform(
                async move { spawn_attach(&command) },
                Message::AttachSpawned,
            )),
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
        text(format!("Unknown project {project_id}")).size(15)
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
        text(label).size(15)
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
    let mut detail = column![text("Session").size(22)].spacing(8);
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
            detail = detail.push(text(format!("cwd: {}", session.cwd.display())).size(14));
            detail = detail.push(button("Open in terminal").on_press(Message::OpenSession {
                host_id: host_id.clone(),
                session_id: session.id.clone(),
            }));
        }
        None => {
            detail = detail.push(text("No session selected").size(16));
        }
    }
    for toast in app.workspace.toasts.iter().rev().take(3).rev() {
        detail = detail.push(toast_view(toast));
    }
    if let Some(status) = &app.status {
        detail = detail.push(text(status).size(13));
    }
    scrollable(detail).into()
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

fn spawn_attach(command: &str) -> Result<(), String> {
    Command::new("sh")
        .arg("-c")
        .arg(command)
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("failed to spawn attach command `{command}`: {err}"))
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
