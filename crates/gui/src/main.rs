//! Native Iced shell for the pohunek control plane.

// Rust guideline compliant 2026-06-26
#![forbid(unsafe_code)]

mod runtime;

use std::path::PathBuf;
use std::process::Command;

use iced::widget::{button, column, container, row, scrollable, text};
use iced::{Element, Fill, Subscription, Task, Theme};
use pohunek_gui_core::{
    render_attach_command, AttachTemplateValues, ConnState, HostConfig, HostId,
    Message as CoreMessage, Workspace,
};
use serde::Deserialize;
use thiserror::Error;

pub fn main() -> iced::Result {
    iced::application(PohunekApp::boot, update, view)
        .subscription(subscription)
        .theme(theme)
        .window_size((960.0, 640.0))
        .run()
}

#[derive(Debug, Clone)]
struct PohunekApp {
    workspace: Workspace,
    config: Result<AppConfig, String>,
    status: Option<String>,
}

impl PohunekApp {
    fn boot() -> (Self, Task<Message>) {
        let config = AppConfig::load().map_err(|err| err.to_string());
        let task = match &config {
            Ok(config) => Task::perform(
                runtime::perform(pohunek_gui_core::load_host(config.local_host.clone())),
                Message::Core,
            ),
            Err(_) => Task::none(),
        };
        (
            Self {
                workspace: Workspace::default(),
                config,
                status: None,
            },
            task,
        )
    }
}

#[derive(Debug, Clone)]
enum Message {
    Core(CoreMessage),
    OpenSession { host_id: HostId, session_id: String },
    AttachSpawned(Result<(), String>),
}

fn update(app: &mut PohunekApp, message: Message) -> Task<Message> {
    match message {
        Message::Core(message) => {
            app.workspace.apply(message);
            Task::none()
        }
        Message::OpenSession {
            host_id,
            session_id,
        } => match app.attach_command(&host_id, &session_id) {
            Ok(command) => Task::perform(
                async move { spawn_attach(&command) },
                Message::AttachSpawned,
            ),
            Err(err) => {
                app.status = Some(err);
                Task::none()
            }
        },
        Message::AttachSpawned(result) => {
            app.status = Some(match result {
                Ok(()) => "attach command spawned".to_owned(),
                Err(err) => err,
            });
            Task::none()
        }
    }
}

impl PohunekApp {
    fn attach_command(&self, host_id: &HostId, session_id: &str) -> Result<String, String> {
        let config = self.config.as_ref().map_err(Clone::clone)?;
        let host = config
            .hosts
            .iter()
            .find(|host| &host.id == host_id)
            .ok_or_else(|| format!("unknown host `{host_id}`"))?;
        Ok(render_attach_command(
            &config.attach_command,
            &AttachTemplateValues {
                bin: config.pohunek_bin.clone(),
                host: host.attach_host().to_owned(),
                id: session_id.to_owned(),
            },
        ))
    }
}

fn subscription(app: &PohunekApp) -> Subscription<Message> {
    let Ok(config) = &app.config else {
        return Subscription::none();
    };
    Subscription::batch(
        config.hosts.iter().cloned().map(|host| {
            Subscription::run_with(host, runtime::host_subscription).map(Message::Core)
        }),
    )
}

fn view(app: &PohunekApp) -> Element<'_, Message> {
    let mut hosts = column![text("Hosts").size(22)].spacing(8);
    match &app.config {
        Ok(_) => {
            for (host_id, host) in &app.workspace.hosts {
                let status = match host.conn {
                    ConnState::Connecting => "connecting",
                    ConnState::Connected => "connected",
                    ConnState::Disconnected => "disconnected",
                    ConnState::Unreachable => "unreachable",
                };
                let label = match &host.health {
                    Some(health) => {
                        format!("{} - {status} - daemon {}", host_id, health.daemon_version)
                    }
                    None => format!("{host_id} - {status}"),
                };
                hosts = hosts.push(text(label).size(16));
                if let Some(error) = &host.last_error {
                    hosts = hosts.push(text(error).size(14));
                }
            }
        }
        Err(err) => {
            hosts = hosts.push(text(format!("configuration error: {err}")).size(16));
        }
    }

    let mut sessions = column![text("Sessions").size(22)].spacing(6);
    for (host_id, host) in &app.workspace.hosts {
        for session in host.sessions.values() {
            let activity = session
                .activity
                .map_or("unknown", |activity| activity.as_str());
            sessions = sessions.push(
                row![
                    text(format!(
                        "{} / {} / {} / {}",
                        host_id, session.id.0, session.agent, activity
                    ))
                    .width(Fill),
                    button("Open").on_press(Message::OpenSession {
                        host_id: host_id.clone(),
                        session_id: session.id.0.clone(),
                    })
                ]
                .spacing(8),
            );
        }
    }
    if app
        .workspace
        .hosts
        .values()
        .all(|host| host.sessions.is_empty())
    {
        sessions = sessions.push(text("No sessions reported").size(16));
    }

    if let Some(status) = &app.status {
        sessions = sessions.push(text(status).size(14));
    }

    container(row![
        container(scrollable(hosts)).width(280),
        container(scrollable(sessions)).width(Fill)
    ])
    .padding(16)
    .width(Fill)
    .height(Fill)
    .into()
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

#[derive(Debug, Clone)]
struct AppConfig {
    attach_command: String,
    pohunek_bin: String,
    local_host: HostConfig,
    hosts: Vec<HostConfig>,
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
        Ok(Self {
            attach_command: raw.attach_command,
            pohunek_bin: raw.pohunek_bin,
            hosts: vec![local_host.clone()],
            local_host,
        })
    }
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    attach_command: String,
    pohunek_bin: String,
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
