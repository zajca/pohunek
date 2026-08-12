//! Selection lookups derived from the current UI selection.

#[cfg(test)]
use std::path::PathBuf;

use iced::Task;
use pohunek_gui_core::{ConnectionOptions, HostConfig, HostId, Selection};
use protocol::{ProjectInfo, ProviderKind, SessionId, SessionInfo};

use crate::config::TerminalSize;
use crate::message::Message;
use crate::PohunekApp;

pub(crate) fn selected_session_target(app: &PohunekApp) -> Result<(HostConfig, SessionId), String> {
    let Some(Selection::Session {
        host_id,
        session_id,
    }) = app.ui_state.selection.clone()
    else {
        return Err("select a session first".to_owned());
    };
    Ok((host_config(app, &host_id)?, session_id))
}

pub(crate) fn sync_rename_edit_for_selection(app: &mut PohunekApp) {
    let Some((_, session)) = selected_session(app) else {
        return;
    };
    app.rename_edit = session.name.clone().unwrap_or_default();
}

pub(crate) fn selected_host_config(app: &PohunekApp) -> Result<HostConfig, String> {
    host_config(app, &selected_host_id(app)?)
}

pub(crate) fn selected_host_id(app: &PohunekApp) -> Result<HostId, String> {
    match app.ui_state.selection.as_ref() {
        Some(
            Selection::Host { host_id }
            | Selection::Project { host_id, .. }
            | Selection::Session { host_id, .. },
        ) => Some(host_id.clone()),
        None => app.hosts.first().map(|host| host.id.clone()),
    }
    .ok_or_else(|| "no host is available yet".to_owned())
}

pub(crate) fn host_config(app: &PohunekApp, host_id: &HostId) -> Result<HostConfig, String> {
    app.hosts
        .iter()
        .find(|host| &host.id == host_id)
        .cloned()
        .ok_or_else(|| format!("unknown host `{host_id}`"))
}

pub(crate) fn selected_project_reference(app: &PohunekApp) -> Result<String, String> {
    match app.ui_state.selection.as_ref() {
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
    }
    .ok_or_else(|| "select a project or project-linked session first".to_owned())
}

#[derive(Debug, Clone)]
pub(crate) struct AssistantProjectTarget {
    pub(crate) host: HostConfig,
    pub(crate) project_ref: String,
}

pub(crate) fn selected_assistant_project(
    app: &PohunekApp,
) -> Result<AssistantProjectTarget, String> {
    let host_id = selected_host_id(app)?;
    Ok(AssistantProjectTarget {
        host: host_config(app, &host_id)?,
        project_ref: selected_project_reference(app)?,
    })
}

#[cfg(test)]
pub(crate) fn selected_project_identity(app: &PohunekApp) -> Result<(String, PathBuf), String> {
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
    .ok_or_else(|| "select a project or project-linked session first".to_owned())
}

pub(crate) fn available_actions(app: &PohunekApp, provider: &ProviderKind) -> Vec<String> {
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

pub(crate) fn connection_options(app: &PohunekApp) -> Result<ConnectionOptions, String> {
    app.config
        .as_ref()
        .map(|config| config.connection_options)
        .map_err(Clone::clone)
}

pub(crate) fn terminal_size(app: &PohunekApp) -> Result<TerminalSize, String> {
    app.config
        .as_ref()
        .map(|config| config.terminal_size)
        .map_err(Clone::clone)
}

pub(crate) fn optional_field(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

pub(crate) fn required_field(value: &str, label: &str) -> Result<String, String> {
    optional_field(value).ok_or_else(|| format!("{label} is required"))
}

pub(crate) fn save_ui_state_task(app: &PohunekApp) -> Task<Message> {
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

pub(crate) fn project_is_selected(app: &PohunekApp, host_id: &HostId, project_id: &str) -> bool {
    matches!(
        app.ui_state.selection.as_ref(),
        Some(Selection::Project { host_id: h, project_id: p }) if h == host_id && p == project_id
    )
}

pub(crate) fn selected_session(app: &PohunekApp) -> Option<(&HostId, &SessionInfo)> {
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

pub(crate) fn selected_project(app: &PohunekApp) -> Option<(&HostId, &ProjectInfo)> {
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
