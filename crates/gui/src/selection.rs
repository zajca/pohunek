//! Selection lookups and provider-filter resolution derived from the current UI selection.

use std::path::PathBuf;

use iced::Task;
use pohunek_gui_core::{
    providers, session_link_metadata, ConnectionOptions, GitHubProviderScope,
    GitHubPullRequestStatusKey, HostConfig, HostId, Selection, SessionLinkKind,
    SessionLinkProvider,
};
use protocol::{ProjectInfo, ProviderKind, SessionId, SessionInfo};

use crate::config::{ConfigError, RawProjectFilters, TerminalSize};
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

pub(crate) fn selected_host_config(app: &PohunekApp) -> Result<HostConfig, String> {
    let host_id = selected_host_id(app)?;
    host_config(app, &host_id)
}

pub(crate) fn selected_host_id(app: &PohunekApp) -> Result<HostId, String> {
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

pub(crate) fn host_config(app: &PohunekApp, host_id: &HostId) -> Result<HostConfig, String> {
    app.hosts
        .iter()
        .find(|host| &host.id == host_id)
        .cloned()
        .ok_or_else(|| format!("unknown host `{host_id}`"))
}

pub(crate) fn selected_project_reference(app: &PohunekApp) -> Result<String, String> {
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
pub(crate) struct AssistantProjectTarget {
    pub(crate) host: HostConfig,
    pub(crate) project_ref: String,
}

pub(crate) fn selected_assistant_project(
    app: &PohunekApp,
) -> Result<AssistantProjectTarget, String> {
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
    .ok_or_else(|| {
        "select a project or linked project session before browsing providers".to_owned()
    })
}

pub(crate) fn selected_github_scope(app: &PohunekApp) -> Result<GitHubProviderScope, String> {
    let (project_id, repo_root) = selected_project_identity(app)?;
    Ok(GitHubProviderScope::new(project_id, repo_root))
}

pub(crate) fn github_client_for_selected_project(
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
pub(crate) fn ensure_project_filters_loaded(app: &mut PohunekApp) {
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
pub(crate) fn effective_filters(app: &PohunekApp) -> providers::filters::ProviderFilterSet {
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
pub(crate) fn active_github_filter(app: &PohunekApp) -> Option<providers::filters::GitHubFilter> {
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
pub(crate) fn active_linear_filter(app: &PohunekApp) -> Option<providers::filters::LinearFilter> {
    let filters = effective_filters(app);
    let selected = selected_host_id(app)
        .ok()
        .and_then(|host_id| app.workspace.hosts.get(&host_id))
        .and_then(|host| host.provider.linear.selected_filter.clone());
    selected
        .and_then(|name| filters.linear_filter(&name).cloned())
        .or_else(|| filters.linear.first().cloned())
}

pub(crate) fn selected_linear_issue(
    app: &PohunekApp,
) -> Result<providers::linear::LinearIssue, String> {
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

pub(crate) fn selected_github_pull_request(
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
pub(crate) struct GitHubStatusTarget {
    pub(crate) number: u64,
    pub(crate) status_key: GitHubPullRequestStatusKey,
}

pub(crate) fn selected_github_pr_status_target(
    app: &PohunekApp,
) -> Result<GitHubStatusTarget, String> {
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

/// Resolves the launch action name for `provider`: the operator's picked action
/// when valid, otherwise the first matching action defined for the project.
pub(crate) fn launch_action_name(
    app: &PohunekApp,
    provider: &ProviderKind,
) -> Result<String, String> {
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

/// Whether the given project is the current selection (drives row highlight).
pub(crate) fn project_is_selected(app: &PohunekApp, host_id: &HostId, project_id: &str) -> bool {
    matches!(
        app.ui_state.selection.as_ref(),
        Some(Selection::Project { host_id: h, project_id: p }) if h == host_id && p == project_id
    )
}

/// Whether the given session is the current selection (drives row highlight).
pub(crate) fn session_is_selected(
    app: &PohunekApp,
    host_id: &HostId,
    session_id: &SessionId,
) -> bool {
    matches!(
        app.ui_state.selection.as_ref(),
        Some(Selection::Session { host_id: h, session_id: s }) if h == host_id && s == session_id
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
