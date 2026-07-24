//! Native Iced shell for the pohunek control plane.

// Rust guideline compliant 2026-07-21
#![forbid(unsafe_code)]

mod attach;
mod command;
mod config;
mod keyboard;
mod message;
mod runtime;
mod selection;
mod view;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use iced::widget::text_editor;
use iced::{window, Subscription, Task, Theme};
use pohunek_gui_core::{
    default_state_dir, providers, AttachTemplateValues, HostConfig, HostId, NotificationFilter,
    NotificationScope, UiState, Workspace,
};
use protocol::{AgentActivity, NotificationId, SessionId};
use thiserror::Error;

use attach::window_dimension_to_f32;
use command::{discover_hosts_task, update};
use config::AppConfig;
use message::{
    AssistantForm, InboxView, Message, MetadataEdit, ModalView, ProjectEdit, StartForm,
    TemplateRecipe,
};
use view::view;

// Wayland clients discover their compositor through this standard variable.
const WAYLAND_DISPLAY_ENV: &str = "WAYLAND_DISPLAY";

// X11 clients use this standard variable; seeing it without Wayland gives a
// clearer error than letting the window backend fail later.
const X11_DISPLAY_ENV: &str = "DISPLAY";

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
    keymap: keyboard::KeyMap,
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
    /// Active inbox host filter; `None` fields do not constrain the notification list.
    notification_filter: NotificationFilter,
    /// `Needs action | All | Archived` scope picked in the inbox modal.
    inbox_scope: NotificationScope,
    /// Which layer of the inbox modal is showing.
    inbox_view: InboxView,
    /// Keyboard cursor for the inbox list layer. This stays local UI state; the
    /// persisted UI selection remains reserved for workspace tree entities.
    inbox_cursor: Option<(HostId, NotificationId)>,
    /// Whether the inbox message layer's `> Details` section is expanded.
    inbox_details_expanded: bool,
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
    /// Cursor into the agents monitor's blocked-session list, advanced by the
    /// `b` keyboard shortcut (`keyboard::route_key_press`) so repeated
    /// presses cycle through every blocked agent instead of reselecting the
    /// first one.
    blocked_cycle_index: usize,
}

impl PohunekApp {
    fn boot(boot: BootState) -> (Self, Task<Message>) {
        let config = AppConfig::load().map_err(|err| err.to_string());
        let keymap = config.as_ref().map_or_else(
            |_| keyboard::KeyMap::default(),
            |config| config.keymap.clone(),
        );
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
                keymap,
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
                state_dir: boot.state_dir,
                status: boot.status,
                notified_intents: 0,
                blocked_cycle_index: 0,
            },
            task,
        )
    }

    pub(crate) fn attach_values(
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

    /// A minimal app for view/state unit tests: no config, no hosts, and all
    /// forms at their defaults. Callers populate `workspace`/`ui_state`/`start`
    /// as their test needs.
    #[cfg(test)]
    pub(crate) fn test_default() -> Self {
        Self {
            workspace: Workspace::default(),
            config: Err("test config is intentionally absent".to_owned()),
            keymap: keyboard::KeyMap::default(),
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
}

fn subscription(app: &PohunekApp) -> Subscription<Message> {
    let mut subscriptions = vec![
        window::resize_events().map(|(_id, size)| Message::WindowResized(size)),
        keyboard::subscription(),
    ];
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

fn theme(_app: &PohunekApp) -> Theme {
    Theme::TokyoNight
}

#[cfg(test)]
mod tests {
    use pohunek_gui_core::{ConnState, Selection};
    use protocol::{ProjectInfo, SessionInfo};

    use super::*;
    use crate::attach::session_requires_resume_before_attach;
    use crate::config::{
        non_empty_config_path, validate_http_endpoint, RawGuiConfig, RawLinearProviderConfig,
    };
    use crate::selection::{
        selected_assistant_project, selected_project_identity, selected_project_reference,
    };
    use crate::view::inbox::{parse_rfc3339_utc_seconds, SECONDS_PER_DAY};

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
            external: Some(false),
            agent: "codex".to_owned(),
            agent_base: protocol::AgentKind::Codex,
            cwd: PathBuf::from("/tmp/project"),
            cwd_source: Some(protocol::CwdSource::Launch),
            pid: 42,
            cols: 80,
            rows: 24,
            state,
            state_source: protocol::StateSource::Process,
            activity: None,
            active_agent: None,
            active_agent_base: None,
            active_agent_pid: None,
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
            runtime: None,
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
            review: pohunek_gui_core::ReviewTabState::default(),
            last_agent_state: None,
            last_error: None,
            supported_agents: Vec::new(),
        };
        host.sessions.insert(session.id.0.clone(), session);

        let mut app = PohunekApp {
            workspace: Workspace::default(),
            config: Err("test config is intentionally absent".to_owned()),
            keymap: keyboard::KeyMap::default(),
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
        };
        app.workspace.hosts.insert(host_id.clone(), host);
        app
    }

    #[test]
    fn terminal_session_resumes_before_attach() {
        let host_id = HostId::new("local");
        let stopped = test_session("s-1", protocol::SessionState::Stopped);
        let running = test_session("s-2", protocol::SessionState::Running);
        let mut lost = test_session("s-3", protocol::SessionState::Running);
        lost.runtime = Some(protocol::SessionRuntime {
            state: protocol::RuntimeState::Lost,
            worker_id: Some("worker-old".to_owned()),
            runtime_id: Some("runtime-old".to_owned()),
            started_at: None,
            last_connected_at: None,
            loss_reason: Some("worker_unavailable".to_owned()),
        });

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
        assert!(session_requires_resume_before_attach(
            &app_with_session(&host_id, lost.clone()),
            &host_id,
            &lost.id
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
            review: pohunek_gui_core::ReviewTabState::default(),
            last_agent_state: None,
            last_error: None,
            supported_agents: Vec::new(),
        };
        host.projects.insert(project.id.clone(), project.clone());

        let mut app = PohunekApp {
            workspace: Workspace::default(),
            config: Err("test config is intentionally absent".to_owned()),
            keymap: keyboard::KeyMap::default(),
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
            inbox_scope: NotificationScope::default(),
            inbox_view: InboxView::default(),
            inbox_cursor: None,
            inbox_details_expanded: false,
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
            blocked_cycle_index: 0,
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
        let mut session = test_session("s-1", protocol::SessionState::Running);
        session.cwd.clone_from(&project.repo_root);
        session.native_session_id = None;
        session.project_id = Some(project.id.clone());
        session.project_label = Some(project.label.clone());
        session.repo = Some(project.repo_root.clone());
        session.branch = Some("main".to_owned());
        session.worktree_path = Some(project.repo_root.clone());
        let mut host = pohunek_gui_core::HostView {
            conn: ConnState::Connected,
            health: None,
            sessions: BTreeMap::new(),
            projects: BTreeMap::new(),
            project_details: BTreeMap::new(),
            notifications: BTreeMap::new(),
            prompt: pohunek_gui_core::PromptState::default(),
            provider: pohunek_gui_core::ProviderState::default(),
            review: pohunek_gui_core::ReviewTabState::default(),
            last_agent_state: None,
            last_error: None,
            supported_agents: Vec::new(),
        };
        host.projects.insert(project.id.clone(), project.clone());
        host.sessions.insert(session.id.0.clone(), session.clone());

        let mut app = PohunekApp {
            workspace: Workspace::default(),
            config: Err("test config is intentionally absent".to_owned()),
            keymap: keyboard::KeyMap::default(),
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
