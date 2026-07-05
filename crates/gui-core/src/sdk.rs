//! Daemon SDK call helpers for sessions, projects, notifications, and prompts.

use std::collections::BTreeMap;

use pohunek_client::Client;
use protocol::{
    method, NotificationDeleteParams, NotificationDeleteResult, NotificationListParams,
    NotificationListResult, NotificationRecord, NotificationStatus, NotificationUpdateParams,
    NotificationUpdateResult, ProjectActionParams, ProjectActionResult, ProjectActionsParams,
    ProjectActionsResult, ProjectAddParams, ProjectInfo, ProjectListParams, ProjectPromptParams,
    ProjectPromptResult, ProjectRemoveParams, ProjectRemoveResult, ProjectRenameParams,
    ProjectShowParams, ProjectShowResult, SessionId, SessionInfo, SessionListParams,
    SessionNewParams, SessionNewResult, SessionRemoveResult, SessionRenameParams,
    SessionRenameResult, SessionResumeResult, SessionSetMetadataParams, SessionSetMetadataResult,
    SessionStopResult, WorktreeRemoveParams, WorktreeRemoveResult,
};

use crate::connection::connect_client;
use crate::{
    preview_action_prompt, ConnectionOptions, CoreError, HealthSummary, HostConfig, HostId,
    HostSnapshot, Message, PromptLaunchParams, ProviderLaunchParams, GUI_NOTIFICATION_SEED_LIMIT,
    METHOD_NOT_FOUND_CODE,
};

/// Load one host snapshot with `daemon.health` and `session.list`.
pub async fn load_host(config: HostConfig) -> Message {
    match load_host_snapshot(&config).await {
        Ok(snapshot) => Message::HostSnapshotLoaded { snapshot },
        Err(err) => Message::HostDisconnected {
            host_id: config.id,
            error: err.to_string(),
        },
    }
}

/// Load one host snapshot and return typed data for headless tests.
pub async fn load_host_snapshot(config: &HostConfig) -> Result<HostSnapshot, CoreError> {
    load_host_snapshot_with_options(config, ConnectionOptions::default()).await
}

/// Create a session on a host through the SDK.
pub async fn create_session(
    config: &HostConfig,
    params: SessionNewParams,
) -> Result<SessionNewResult, CoreError> {
    create_session_with_options(config, params, ConnectionOptions::default()).await
}

/// Create a session with explicit connection options.
pub async fn create_session_with_options(
    config: &HostConfig,
    params: SessionNewParams,
    options: ConnectionOptions,
) -> Result<SessionNewResult, CoreError> {
    call_host::<method::SessionNew>(config, options, params).await
}

/// Inspect a session on a host through the SDK.
pub async fn inspect_session(
    config: &HostConfig,
    session_id: &SessionId,
) -> Result<SessionInfo, CoreError> {
    inspect_session_with_options(config, session_id, ConnectionOptions::default()).await
}

/// Inspect a session with explicit connection options.
pub async fn inspect_session_with_options(
    config: &HostConfig,
    session_id: &SessionId,
    options: ConnectionOptions,
) -> Result<SessionInfo, CoreError> {
    call_host::<method::SessionInspect>(config, options, session_id.clone()).await
}

/// Resume a terminal session on a host through the SDK.
pub async fn resume_session(
    config: &HostConfig,
    session_id: &SessionId,
) -> Result<SessionResumeResult, CoreError> {
    resume_session_with_options(config, session_id, ConnectionOptions::default()).await
}

/// Resume a terminal session with explicit connection options.
pub async fn resume_session_with_options(
    config: &HostConfig,
    session_id: &SessionId,
    options: ConnectionOptions,
) -> Result<SessionResumeResult, CoreError> {
    call_host::<method::SessionResume>(config, options, session_id.clone()).await
}

/// Stop a session on a host through the SDK.
pub async fn stop_session(
    config: &HostConfig,
    session_id: &SessionId,
) -> Result<SessionStopResult, CoreError> {
    stop_session_with_options(config, session_id, ConnectionOptions::default()).await
}

/// Stop a session with explicit connection options.
pub async fn stop_session_with_options(
    config: &HostConfig,
    session_id: &SessionId,
    options: ConnectionOptions,
) -> Result<SessionStopResult, CoreError> {
    call_host::<method::SessionStop>(config, options, session_id.clone()).await
}

/// Remove a session from a host through the SDK.
///
/// Removal stops a still-live session first, then evicts it from the daemon's
/// registry so it stops appearing in `list`.
pub async fn remove_session(
    config: &HostConfig,
    session_id: &SessionId,
) -> Result<SessionRemoveResult, CoreError> {
    remove_session_with_options(config, session_id, ConnectionOptions::default()).await
}

/// Remove a session with explicit connection options.
pub async fn remove_session_with_options(
    config: &HostConfig,
    session_id: &SessionId,
    options: ConnectionOptions,
) -> Result<SessionRemoveResult, CoreError> {
    call_host::<method::SessionRemove>(config, options, session_id.clone()).await
}

/// Merge or clear session metadata on a host.
pub async fn set_session_metadata(
    config: &HostConfig,
    params: SessionSetMetadataParams,
) -> Result<SessionSetMetadataResult, CoreError> {
    set_session_metadata_with_options(config, params, ConnectionOptions::default()).await
}

/// Merge or clear metadata with explicit connection options.
pub async fn set_session_metadata_with_options(
    config: &HostConfig,
    params: SessionSetMetadataParams,
    options: ConnectionOptions,
) -> Result<SessionSetMetadataResult, CoreError> {
    call_host::<method::SessionSetMetadata>(config, options, params).await
}

/// Set or clear a session's display name on a host.
pub async fn rename_session(
    config: &HostConfig,
    params: SessionRenameParams,
) -> Result<SessionRenameResult, CoreError> {
    rename_session_with_options(config, params, ConnectionOptions::default()).await
}

/// Set or clear a session's display name with explicit connection options.
pub async fn rename_session_with_options(
    config: &HostConfig,
    params: SessionRenameParams,
    options: ConnectionOptions,
) -> Result<SessionRenameResult, CoreError> {
    call_host::<method::SessionRename>(config, options, params).await
}

/// List notification records on a host through the SDK.
pub async fn list_notifications(
    config: &HostConfig,
    params: NotificationListParams,
) -> Result<NotificationListResult, CoreError> {
    list_notifications_with_options(config, params, ConnectionOptions::default()).await
}

/// List notification records with explicit connection options.
pub async fn list_notifications_with_options(
    config: &HostConfig,
    params: NotificationListParams,
    options: ConnectionOptions,
) -> Result<NotificationListResult, CoreError> {
    call_host::<method::NotificationList>(config, options, params).await
}

/// Update a notification's lifecycle status on a host.
pub async fn update_notification(
    config: &HostConfig,
    params: NotificationUpdateParams,
) -> Result<NotificationUpdateResult, CoreError> {
    update_notification_with_options(config, params, ConnectionOptions::default()).await
}

/// Update a notification with explicit connection options.
pub async fn update_notification_with_options(
    config: &HostConfig,
    params: NotificationUpdateParams,
    options: ConnectionOptions,
) -> Result<NotificationUpdateResult, CoreError> {
    call_host::<method::NotificationUpdate>(config, options, params).await
}

/// Delete a notification record on a host.
pub async fn delete_notification(
    config: &HostConfig,
    params: NotificationDeleteParams,
) -> Result<NotificationDeleteResult, CoreError> {
    delete_notification_with_options(config, params, ConnectionOptions::default()).await
}

/// Delete a notification with explicit connection options.
pub async fn delete_notification_with_options(
    config: &HostConfig,
    params: NotificationDeleteParams,
    options: ConnectionOptions,
) -> Result<NotificationDeleteResult, CoreError> {
    call_host::<method::NotificationDelete>(config, options, params).await
}

/// List projects on a host through the SDK.
pub async fn list_projects(config: &HostConfig) -> Result<Vec<ProjectInfo>, CoreError> {
    list_projects_with_options(config, ConnectionOptions::default()).await
}

/// List projects with explicit connection options.
pub async fn list_projects_with_options(
    config: &HostConfig,
    options: ConnectionOptions,
) -> Result<Vec<ProjectInfo>, CoreError> {
    call_host::<method::ProjectList>(
        config,
        options,
        ProjectListParams {
            filters: Vec::new(),
        },
    )
    .await
}

/// Add a project on a host through the SDK.
pub async fn add_project(
    config: &HostConfig,
    params: ProjectAddParams,
) -> Result<ProjectInfo, CoreError> {
    add_project_with_options(config, params, ConnectionOptions::default()).await
}

/// Add a project with explicit connection options.
pub async fn add_project_with_options(
    config: &HostConfig,
    params: ProjectAddParams,
    options: ConnectionOptions,
) -> Result<ProjectInfo, CoreError> {
    call_host::<method::ProjectAdd>(config, options, params).await
}

/// Show a project and its live worktrees.
pub async fn show_project(
    config: &HostConfig,
    params: ProjectShowParams,
) -> Result<ProjectShowResult, CoreError> {
    show_project_with_options(config, params, ConnectionOptions::default()).await
}

/// Show a project with explicit connection options.
pub async fn show_project_with_options(
    config: &HostConfig,
    params: ProjectShowParams,
    options: ConnectionOptions,
) -> Result<ProjectShowResult, CoreError> {
    call_host::<method::ProjectShow>(config, options, params).await
}

/// Rename a project on a host through the SDK.
pub async fn rename_project(
    config: &HostConfig,
    params: ProjectRenameParams,
) -> Result<ProjectInfo, CoreError> {
    rename_project_with_options(config, params, ConnectionOptions::default()).await
}

/// Rename a project with explicit connection options.
pub async fn rename_project_with_options(
    config: &HostConfig,
    params: ProjectRenameParams,
    options: ConnectionOptions,
) -> Result<ProjectInfo, CoreError> {
    call_host::<method::ProjectRename>(config, options, params).await
}

/// Remove a project from a host.
pub async fn remove_project(
    config: &HostConfig,
    params: ProjectRemoveParams,
) -> Result<ProjectRemoveResult, CoreError> {
    remove_project_with_options(config, params, ConnectionOptions::default()).await
}

/// Remove a project with explicit connection options.
pub async fn remove_project_with_options(
    config: &HostConfig,
    params: ProjectRemoveParams,
    options: ConnectionOptions,
) -> Result<ProjectRemoveResult, CoreError> {
    call_host::<method::ProjectRemove>(config, options, params).await
}

/// Remove a single pohunek-owned worktree from a host.
pub async fn remove_worktree(
    config: &HostConfig,
    params: WorktreeRemoveParams,
) -> Result<WorktreeRemoveResult, CoreError> {
    remove_worktree_with_options(config, params, ConnectionOptions::default()).await
}

/// Remove a single worktree with explicit connection options.
pub async fn remove_worktree_with_options(
    config: &HostConfig,
    params: WorktreeRemoveParams,
    options: ConnectionOptions,
) -> Result<WorktreeRemoveResult, CoreError> {
    call_host::<method::WorktreeRemove>(config, options, params).await
}

/// List project actions on a host through the SDK.
pub async fn list_project_actions(
    config: &HostConfig,
    params: ProjectActionsParams,
) -> Result<ProjectActionsResult, CoreError> {
    list_project_actions_with_options(config, params, ConnectionOptions::default()).await
}

/// List project actions with explicit connection options.
pub async fn list_project_actions_with_options(
    config: &HostConfig,
    params: ProjectActionsParams,
    options: ConnectionOptions,
) -> Result<ProjectActionsResult, CoreError> {
    call_host::<method::ProjectActions>(config, options, params).await
}

/// Resolve a project prompt on a host through the SDK.
pub async fn resolve_project_prompt(
    config: &HostConfig,
    params: ProjectPromptParams,
) -> Result<ProjectPromptResult, CoreError> {
    resolve_project_prompt_with_options(config, params, ConnectionOptions::default()).await
}

/// Resolve a project prompt with explicit connection options.
pub async fn resolve_project_prompt_with_options(
    config: &HostConfig,
    params: ProjectPromptParams,
    options: ConnectionOptions,
) -> Result<ProjectPromptResult, CoreError> {
    call_host::<method::ProjectPrompt>(config, options, params).await
}

/// Resolve a project action on a host through the SDK.
pub async fn resolve_project_action(
    config: &HostConfig,
    params: ProjectActionParams,
) -> Result<ProjectActionResult, CoreError> {
    resolve_project_action_with_options(config, params, ConnectionOptions::default()).await
}

/// Resolve a project action with explicit connection options.
pub async fn resolve_project_action_with_options(
    config: &HostConfig,
    params: ProjectActionParams,
    options: ConnectionOptions,
) -> Result<ProjectActionResult, CoreError> {
    call_host::<method::ProjectAction>(config, options, params).await
}

/// Launch a rendered action prompt on a host.
pub async fn launch_action_prompt_with_options(
    config: &HostConfig,
    params: PromptLaunchParams,
    options: ConnectionOptions,
) -> Result<SessionNewResult, CoreError> {
    let branch = params
        .preview
        .branch
        .clone()
        .or_else(|| params.action.branch.clone());
    create_session_with_options(
        config,
        SessionNewParams {
            agent: params.action.agent,
            name: params.name,
            cwd: None,
            cols: params.cols,
            rows: params.rows,
            project: Some(params.project),
            repo: None,
            branch,
            base_branch: params.action.base_branch,
            input: Some(params.preview.rendered),
            metadata: params.metadata,
        },
        options,
    )
    .await
}

/// Resolve a provider action, render its prompt, and launch exactly one linked session.
pub async fn launch_provider_item_with_options(
    config: &HostConfig,
    params: ProviderLaunchParams,
    options: ConnectionOptions,
) -> Result<SessionNewResult, CoreError> {
    params.item.validate_link_invariants()?;
    let action = resolve_project_action_with_options(
        config,
        ProjectActionParams {
            reference: params.project.clone(),
            name: params.action_name,
        },
        options,
    )
    .await?;
    if action.provider != params.item.action_provider {
        return Err(CoreError::ProviderActionMismatch {
            expected: params.item.action_provider.as_str(),
            actual: action.provider.as_str(),
        });
    }

    let preview = preview_action_prompt(
        &action,
        params.item.item_id.clone(),
        params.item.context_json.clone(),
    )?;
    let branch = preview
        .branch
        .clone()
        .or_else(|| action.branch.clone())
        .ok_or(CoreError::MissingPromptBranch {
            provider: params.item.prompt_provider.as_str(),
        })?;
    let link = params.item.to_session_link(branch)?;
    launch_action_prompt_with_options(
        config,
        PromptLaunchParams {
            project: params.project,
            action,
            preview,
            cols: params.cols,
            rows: params.rows,
            metadata: link.to_session_metadata(),
            name: params.name,
        },
        options,
    )
    .await
}

pub(crate) async fn load_host_snapshot_with_options(
    config: &HostConfig,
    options: ConnectionOptions,
) -> Result<HostSnapshot, CoreError> {
    let mut client = connect_client(config, options).await?;
    let health = HealthSummary::from(call_client::<method::DaemonHealth>(&mut client, ()).await?);
    let sessions = call_client::<method::SessionList>(
        &mut client,
        SessionListParams {
            filters: Vec::new(),
        },
    )
    .await?;
    let projects = match call_client::<method::ProjectList>(
        &mut client,
        ProjectListParams {
            filters: Vec::new(),
        },
    )
    .await
    {
        Ok(projects) => (projects, None),
        Err(err) => (Vec::new(), Some(format!("project.list failed: {err}"))),
    };
    let notifications = load_host_notifications(&mut client, &config.id).await;
    Ok(HostSnapshot {
        host_id: config.id.clone(),
        health,
        sessions,
        projects: projects.0,
        project_error: combine_seed_errors(projects.1, notifications.1),
        notifications: notifications.0,
    })
}

/// Seed recent notifications for one host, deduped across the seed queries.
///
/// Seeding is non-fatal: a host daemon without the notification surface answers
/// `method_not_found`, which is logged and treated as an empty inbox so the host
/// still connects. Runtime failures are surfaced through the snapshot's existing
/// degraded-status error channel. The daemon does not poison the connection on
/// a handled error, so this reuses the snapshot client after
/// `session.list`/`project.list`.
async fn load_host_notifications(
    client: &mut Client,
    host_id: &HostId,
) -> (Vec<NotificationRecord>, Option<String>) {
    let mut records: BTreeMap<String, NotificationRecord> = BTreeMap::new();
    let mut first_error = None;
    for params in notification_seed_queries() {
        match call_client::<method::NotificationList>(client, params).await {
            Ok(result) => {
                for record in result.notifications {
                    records.insert(record.id.0.clone(), record);
                }
            }
            Err(err) => {
                if notification_seed_unsupported(&err) {
                    tracing::event!(
                        name: "gui.notification_seed.unsupported",
                        tracing::Level::DEBUG,
                        host_id = %host_id,
                        error = %err,
                        "notification seed unsupported; treating as empty inbox"
                    );
                    return (Vec::new(), None);
                }
                tracing::event!(
                    name: "gui.notification_seed.query.failed",
                    tracing::Level::WARN,
                    host_id = %host_id,
                    error = %err,
                    "notification seed query failed; marking inbox degraded"
                );
                first_error.get_or_insert_with(|| format!("notification.list failed: {err}"));
            }
        }
    }
    (records.into_values().collect(), first_error)
}

fn combine_seed_errors(first: Option<String>, second: Option<String>) -> Option<String> {
    match (first, second) {
        (Some(first), Some(second)) => Some(format!("{first}; {second}")),
        (Some(error), None) | (None, Some(error)) => Some(error),
        (None, None) => None,
    }
}

fn notification_seed_unsupported(err: &CoreError) -> bool {
    match err {
        CoreError::Client(
            pohunek_client::ClientError::Protocol(err)
            | pohunek_client::ClientError::RemoteProtocol { source: err, .. },
        )
        | CoreError::Protocol(err) => {
            err.class == protocol::ErrorClass::Daemon && err.code == METHOD_NOT_FOUND_CODE
        }
        _ => false,
    }
}

/// Seed queries run on connect and reconcile: recent unread first so an unread
/// backlog is never crowded out, then recent records of default live statuses
/// for read/archived context, then a bounded deleted tombstone window.
///
/// The tombstone query is intentionally limited to [`GUI_NOTIFICATION_SEED_LIMIT`]:
/// reconnect only reconciles deletes still covered by the daemon's recent
/// deleted window. Live delete events remain the authoritative path while the
/// GUI is connected, and seed reconciliation never raises OS intents.
pub(crate) fn notification_seed_queries() -> [NotificationListParams; 3] {
    [
        NotificationListParams {
            status: Some(NotificationStatus::Unread),
            limit: Some(GUI_NOTIFICATION_SEED_LIMIT),
            ..NotificationListParams::default()
        },
        NotificationListParams {
            limit: Some(GUI_NOTIFICATION_SEED_LIMIT),
            ..NotificationListParams::default()
        },
        NotificationListParams {
            status: Some(NotificationStatus::Deleted),
            limit: Some(GUI_NOTIFICATION_SEED_LIMIT),
            ..NotificationListParams::default()
        },
    ]
}

/// Each GUI command opens a short-lived client so reconnect state is localized
/// to the operation and does not share failure state with subscriptions.
async fn call_host<M>(
    config: &HostConfig,
    options: ConnectionOptions,
    params: M::Params,
) -> Result<M::Output, CoreError>
where
    M: protocol::Method,
{
    tracing::event!(
        name: "gui.host_request.client.open",
        tracing::Level::DEBUG,
        host_id = %config.id,
        method = M::NAME,
        "opening per-request GUI host client"
    );
    let mut client = match connect_client(config, options).await {
        Ok(client) => client,
        Err(err) => {
            tracing::event!(
                name: "gui.host_request.connect.failed",
                tracing::Level::WARN,
                host_id = %config.id,
                method = M::NAME,
                error = %err,
                "GUI host request connection failed"
            );
            return Err(err);
        }
    };
    match client.call::<M>(params).await {
        Ok(value) => {
            tracing::event!(
                name: "gui.host_request.completed",
                tracing::Level::DEBUG,
                host_id = %config.id,
                method = M::NAME,
                "GUI host request completed"
            );
            Ok(value)
        }
        Err(err) => {
            tracing::event!(
                name: "gui.host_request.failed",
                tracing::Level::WARN,
                host_id = %config.id,
                method = M::NAME,
                error = %err,
                "GUI host request failed"
            );
            Err(err.into())
        }
    }
}

pub(crate) async fn call_client<M>(
    client: &mut Client,
    params: M::Params,
) -> Result<M::Output, CoreError>
where
    M: protocol::Method,
{
    Ok(client.call::<M>(params).await?)
}
