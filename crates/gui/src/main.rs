//! Native Iced shell for the pohunek control plane.

// Rust guideline compliant 2026-09-11
#![forbid(unsafe_code)]

mod attach;
mod bin_resolver;
mod command;
mod config;
mod keyboard;
mod message;
mod notify;
mod runtime;
mod selection;
mod terminal;
#[cfg(test)]
mod test_support;
mod view;

#[cfg(target_os = "linux")]
use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use iced::widget::text_editor;
use iced::{window, Subscription, Task, Theme};
use pohunek_gui_core::{
    default_state_dir, AttachTemplateValues, HostConfig, HostId, NotificationFilter,
    NotificationScope, ProjectRef, UiState, Workspace,
};
use protocol::{NotificationId, SessionId};
use thiserror::Error;

use attach::{window_dimension_to_f32, AttachPlan};
use command::{discover_hosts_task, update};
use config::AppConfig;
use message::{
    AssistantForm, FormField, FormSelect, InboxView, Message, MetadataEdit, ModalView, StartForm,
    TemplateRecipe,
};
use view::view;

// The Wayland-only startup contract applies to Linux; macOS uses the native
// window backend and has no display-server environment to validate.
// Wayland clients discover their compositor through this standard variable.
#[cfg(target_os = "linux")]
const WAYLAND_DISPLAY_ENV: &str = "WAYLAND_DISPLAY";

// X11 clients use this standard variable; seeing it without Wayland gives a
// clearer error than letting the window backend fail later.
#[cfg(target_os = "linux")]
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
    #[cfg(target_os = "linux")]
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
    #[cfg(target_os = "linux")]
    #[error(transparent)]
    Display(#[from] DisplayServerError),
    #[error(transparent)]
    Iced(#[from] iced::Error),
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayServerError {
    MissingWayland,
    X11WithoutWayland,
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
impl std::error::Error for DisplayServerError {}

#[cfg(target_os = "linux")]
fn validate_wayland_environment() -> Result<(), DisplayServerError> {
    let wayland_display = std::env::var_os(WAYLAND_DISPLAY_ENV);
    let x11_display = std::env::var_os(X11_DISPLAY_ENV);
    validate_wayland_display(wayland_display.as_deref(), x11_display.as_deref())
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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
    /// Logical focus for launch-form fields that Iced cannot focus natively.
    form_focus: FormField,
    /// Expanded launch-form select and its keyboard cursor.
    form_select: Option<FormSelect>,
    /// Editable session input / rendered prompt buffer shown in the Start modal.
    prompt_editor: text_editor::Content,
    /// Editable request buffer shown in the Assistant modal.
    assistant_editor: text_editor::Content,
    /// Resolved recipe (agent/branch) for the selected template; `None` when a
    /// blank session or while a template is still resolving.
    template_recipe: Option<TemplateRecipe>,
    /// Which modal, if any, is currently open over the workspace.
    modal: ModalView,
    /// Active inbox host filter; `None` fields do not constrain the notification list.
    notification_filter: NotificationFilter,
    /// `Recent | Unread | Archived` scope picked in the activity modal.
    inbox_scope: NotificationScope,
    /// Which layer of the inbox modal is showing.
    inbox_view: InboxView,
    /// Keyboard cursor for the inbox list layer. This stays local UI state; the
    /// persisted UI selection remains reserved for the selected session.
    inbox_cursor: Option<(HostId, NotificationId)>,
    /// Whether the inbox message layer's `> Details` section is expanded.
    inbox_details_expanded: bool,
    metadata_edit: MetadataEdit,
    /// Edit buffer for renaming the selected session's display name.
    rename_edit: String,
    /// Project the session overview is narrowed to; `None` shows every session.
    project_filter: Option<ProjectRef>,
    /// Counter bumped whenever the Start form's project, template or contents
    /// are reset; a template reply is applied only when it carries the current
    /// value.
    template_generation: u64,
    state_dir: Option<PathBuf>,
    status: Option<String>,
    notified_intents: usize,
    /// Last notification backend state, so failures reach the status line once.
    notification_health: notify::NotificationHealth,
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
                form_focus: FormField::StartAgent,
                form_select: None,
                prompt_editor: text_editor::Content::new(),
                assistant_editor: text_editor::Content::new(),
                template_recipe: None,
                modal: ModalView::None,
                notification_filter: NotificationFilter::default(),
                inbox_scope: NotificationScope::default(),
                inbox_view: InboxView::default(),
                inbox_cursor: None,
                inbox_details_expanded: false,
                metadata_edit: MetadataEdit::default(),
                rename_edit: String::new(),
                project_filter: None,
                template_generation: 0,
                state_dir: boot.state_dir,
                status: boot.status,
                notified_intents: 0,
                notification_health: notify::NotificationHealth::default(),
            },
            task,
        )
    }

    pub(crate) fn attach_plan(
        &self,
        host_id: &HostId,
        session_id: &SessionId,
    ) -> Result<AttachPlan, String> {
        let config = self.config.as_ref().map_err(Clone::clone)?;
        let host = self
            .hosts
            .iter()
            .find(|host| &host.id == host_id)
            .ok_or_else(|| format!("unknown host `{host_id}`"))?;
        Ok(AttachPlan {
            selection: config.attach.clone(),
            resolver: Arc::clone(&config.bin_resolver),
            launch: config.launch,
            values: AttachTemplateValues {
                bin: config.pohunek_bin.clone(),
                host: host.attach_host(),
                id: session_id.0.clone(),
            },
        })
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
            form_focus: FormField::StartAgent,
            form_select: None,
            prompt_editor: text_editor::Content::new(),
            assistant_editor: text_editor::Content::new(),
            template_recipe: None,
            modal: ModalView::None,
            notification_filter: NotificationFilter::default(),
            inbox_scope: NotificationScope::default(),
            inbox_view: InboxView::default(),
            inbox_cursor: None,
            inbox_details_expanded: false,
            metadata_edit: MetadataEdit::default(),
            rename_edit: String::new(),
            project_filter: None,
            template_generation: 0,
            state_dir: None,
            status: None,
            notified_intents: 0,
            notification_health: notify::NotificationHealth::default(),
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
    #[cfg(target_os = "linux")]
    use super::*;
    use crate::config::RawGuiConfig;
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

    #[test]
    fn gui_manifest_keeps_wayland_features_linux_only() {
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
        assert!(!features
            .iter()
            .any(|feature| feature.as_str() == Some("wayland")));
        assert!(!features
            .iter()
            .any(|feature| feature.as_str() == Some("x11")));

        let linux_iced = manifest
            .get("target")
            .and_then(toml::Value::as_table)
            .and_then(|targets| targets.get("cfg(target_os = \"linux\")"))
            .and_then(toml::Value::as_table)
            .and_then(|target| target.get("dependencies"))
            .and_then(toml::Value::as_table)
            .and_then(|dependencies| dependencies.get("iced"))
            .expect("Linux Iced dependency");
        let linux_features = linux_iced
            .get("features")
            .and_then(toml::Value::as_array)
            .expect("explicit Linux Iced features");
        assert!(linux_features
            .iter()
            .any(|feature| feature.as_str() == Some("wayland")));
        assert!(!linux_features
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

    #[cfg(target_os = "linux")]
    #[test]
    fn wayland_guard_accepts_nonempty_wayland_display() {
        validate_wayland_display(Some(std::ffi::OsStr::new("wayland-1")), None)
            .expect("wayland display");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wayland_guard_rejects_missing_wayland_display() {
        let err = validate_wayland_display(None, None).expect_err("missing wayland display");

        assert!(err.to_string().contains("WAYLAND_DISPLAY"));
        assert!(err.to_string().contains("Wayland-only"));
    }

    #[cfg(target_os = "linux")]
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
}
