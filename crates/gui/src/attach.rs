//! Terminal attach/resume plumbing and window-size unit conversions.

use std::process::Command;

use iced::Task;
use pohunek_gui_core::{
    spawn_attach_command, AttachCommandSpawner, AttachTemplateValues, HostId, NotificationIntent,
};
use protocol::SessionId;

use crate::command::resume_session_task;
use crate::message::Message;
use crate::PohunekApp;

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
pub(crate) fn attach_task(
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

pub(crate) fn session_requires_resume_before_attach(
    app: &PohunekApp,
    host_id: &HostId,
    session_id: &SessionId,
) -> bool {
    app.workspace
        .hosts
        .get(host_id)
        .and_then(|host| host.sessions.get(&session_id.0))
        .is_some_and(|session| {
            session.state.is_terminal()
                || session
                    .runtime
                    .as_ref()
                    .is_some_and(|runtime| runtime.state == protocol::RuntimeState::Lost)
        })
}

pub(crate) fn spawn_notification(command: &str, intent: &NotificationIntent) -> Result<(), String> {
    Command::new(command)
        .arg(&intent.title)
        .arg(&intent.body)
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("failed to spawn notification command `{command}`: {err}"))
}

/// Opens `url` via `command`, spawned as a single argv argument.
///
/// Always argv-spawned (`Command::new(command).arg(url)`), never through a
/// shell, so a provider-supplied URL cannot inject shell syntax.
pub(crate) fn spawn_open_url(command: &str, url: &str) -> Result<(), String> {
    Command::new(command)
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("failed to open URL `{url}` with `{command}`: {err}"))
}

pub(crate) fn window_dimension_to_f32(value: u32) -> f32 {
    f32::from(u16::try_from(value).unwrap_or(u16::MAX))
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Iced reports positive window pixel sizes as f32; UI state persists integer pixels"
)]
pub(crate) fn window_dimension_to_u32(value: f32) -> u32 {
    value.round().clamp(1.0, f32::from(u16::MAX)) as u32
}
