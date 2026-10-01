//! Selection lookups derived from the current UI selection.

use iced::Task;
use pohunek_gui_core::{ConnectionOptions, HostConfig, HostId, HostView, ProjectRef, Selection};
use protocol::{ProviderKind, SessionId, SessionInfo};

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

pub(crate) fn host_config(app: &PohunekApp, host_id: &HostId) -> Result<HostConfig, String> {
    app.hosts
        .iter()
        .find(|host| &host.id == host_id)
        .cloned()
        .ok_or_else(|| format!("unknown host `{host_id}`"))
}

/// Host connection plus the project reference a launch form targets.
#[derive(Debug, Clone)]
pub(crate) struct ProjectTarget {
    pub(crate) host: HostConfig,
    pub(crate) project_ref: String,
}

/// Resolves the project chosen in a launch form to its host and wire reference.
/// The project must still be listed by its host.
pub(crate) fn project_target(
    app: &PohunekApp,
    project: Option<&ProjectRef>,
) -> Result<ProjectTarget, String> {
    let project = project.ok_or_else(|| "choose a project first".to_owned())?;
    let host = host_config(app, &project.host_id)?;
    if project_host(app, Some(project)).is_none() {
        return Err(format!(
            "project `{}` is no longer available on host `{}`",
            project.project_id, project.host_id
        ));
    }
    Ok(ProjectTarget {
        host,
        project_ref: project.project_id.clone(),
    })
}

/// Live view of the host that owns the project chosen in a launch form; `None`
/// when the host is unknown or no longer lists the project.
pub(crate) fn project_host<'a>(
    app: &'a PohunekApp,
    project: Option<&ProjectRef>,
) -> Option<&'a HostView> {
    let project = project?;
    app.workspace
        .hosts
        .get(&project.host_id)
        .filter(|host| host.projects.contains_key(&project.project_id))
}

/// Project a freshly opened launch modal starts with: the active session
/// filter, else the selected session's project, else the only known project.
/// Candidates the launch picker does not offer are skipped.
pub(crate) fn preselected_project(app: &PohunekApp) -> Option<ProjectRef> {
    let choices = app.workspace.project_choices();
    let offered = |project: &ProjectRef| choices.iter().any(|choice| &choice.project == project);
    let selected_session_project = selected_session(app).and_then(|(host_id, session)| {
        session.project_id.as_ref().map(|project_id| ProjectRef {
            host_id: host_id.clone(),
            project_id: project_id.clone(),
        })
    });
    app.project_filter
        .clone()
        .filter(offered)
        .or_else(|| selected_session_project.filter(offered))
        .or_else(|| match choices.as_slice() {
            [only] => Some(only.project.clone()),
            _ => None,
        })
}

/// Template (`None`-provider) action names loaded for the Start form's project.
pub(crate) fn available_actions(app: &PohunekApp, provider: &ProviderKind) -> Vec<String> {
    let Some(project) = app.start.project.as_ref() else {
        return Vec::new();
    };
    app.workspace
        .hosts
        .get(&project.host_id)
        .and_then(|host| host.prompt.actions_by_project.get(&project.project_id))
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
