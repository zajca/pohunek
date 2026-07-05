//! The Iced `update` reducer and the command/task builders it dispatches to.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use iced::widget::text_editor;
use iced::Task;
use pohunek_gui_core::assistant::{AssistantPaths, LaunchParams as AssistantLaunchParams};
use pohunek_gui_core::{
    add_project_with_options, assistant as assistant_core, create_session_with_options,
    delete_notification_with_options, discover_hosts, inspect_session_with_options,
    launch_provider_item_with_options, list_project_actions_with_options, preview_action_prompt,
    providers, remove_session_with_options, remove_worktree_with_options,
    rename_project_with_options, rename_session_with_options, resolve_project_action_with_options,
    resume_session_with_options, set_session_metadata_with_options, show_project_with_options,
    stop_session_with_options, update_notification_with_options, ConnectionOptions, HostConfig,
    HostId, Message as CoreMessage, ProviderLaunchItem, ProviderLaunchParams, ProviderOperation,
    ProviderPanel, ProviderRequestId, RightTab, Selection, SessionLinkProvider, WindowSize,
};
use protocol::{
    NotificationDeleteParams, NotificationId, NotificationStatus, NotificationUpdateParams,
    ProjectActionParams, ProjectActionsParams, ProjectAddParams, ProjectRenameParams,
    ProjectShowParams, ProviderKind, SessionId, SessionNewParams, SessionRenameParams,
    SessionSetMetadataParams, WorktreeRemoveParams,
};

use crate::attach::{attach_task, spawn_notification, spawn_open_url, window_dimension_to_u32};
use crate::config::AppConfig;
use crate::keyboard;
use crate::message::{
    AgentChoice, AssistantForm, DiscoveryResult, InboxView, Message, ModalView, NotificationAction,
    ResolvedTemplate, StartForm, TemplateRecipe, ASSISTANT_AUTO_AGENT_LABEL, BLANK_TEMPLATE_LABEL,
};
use crate::runtime;
use crate::selection::{
    active_github_filter, active_linear_filter, connection_options, ensure_project_filters_loaded,
    github_client_for_selected_project, host_config, launch_action_name, optional_field,
    required_field, save_ui_state_task, selected_assistant_project,
    selected_github_pr_status_target, selected_github_pull_request, selected_host_config,
    selected_host_id, selected_linear_issue, selected_project, selected_project_identity,
    selected_project_reference, selected_session_target, sync_rename_edit_for_selection,
    terminal_size,
};
use crate::PohunekApp;

// A second click on the same session within this window counts as a double-click
// and opens the session in a terminal (matching the desktop double-click idiom).
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);

#[expect(
    clippy::too_many_lines,
    reason = "Iced update centralizes shell messages and delegates domain transitions to gui-core"
)]
pub(crate) fn update(app: &mut PohunekApp, message: Message) -> Task<Message> {
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
        Message::SelectTab(tab) => {
            app.ui_state.active_tab = tab;
            tasks.push(save_ui_state_task(app));
            // Auto-fetch the tab's data so switching in immediately shows
            // results instead of requiring a separate Fetch click, matching
            // the pre-B2 Linear|GitHub toggle's behavior.
            match tab {
                RightTab::Linear => {
                    ensure_project_filters_loaded(app);
                    if let Ok(request_id) = begin_linear_issues_request(app) {
                        push_provider_task_result(
                            app,
                            &mut tasks,
                            SessionLinkProvider::Linear,
                            ProviderOperation::LinearIssues,
                            Some(request_id),
                            fetch_linear_issues_task(app, request_id),
                        );
                    }
                }
                RightTab::GitHub => {
                    ensure_project_filters_loaded(app);
                    if let Ok(request_id) = begin_github_pull_requests_request(app) {
                        push_provider_task_result(
                            app,
                            &mut tasks,
                            SessionLinkProvider::GitHub,
                            ProviderOperation::GitHubPullRequests,
                            Some(request_id),
                            fetch_github_pull_requests_task(app, request_id),
                        );
                    }
                    if let Ok(request_id) = begin_github_issues_request(app) {
                        push_provider_task_result(
                            app,
                            &mut tasks,
                            SessionLinkProvider::GitHub,
                            ProviderOperation::GitHubIssues,
                            Some(request_id),
                            fetch_github_issues_task(app, request_id),
                        );
                    }
                }
                RightTab::Detail | RightTab::Worktrees => {}
            }
        }
        Message::FilterActivity(activity) => app.activity_filter = activity,
        Message::OpenInbox => {
            app.modal = ModalView::Inbox;
            app.inbox_view = InboxView::List;
            app.notification_filter.host_id = None;
        }
        Message::OpenHostInbox(host_id) => {
            app.modal = ModalView::Inbox;
            app.inbox_view = InboxView::List;
            app.notification_filter.host_id = Some(host_id);
        }
        Message::SetInboxScope(scope) => app.inbox_scope = scope,
        Message::FilterNotificationHost(host_id) => app.notification_filter.host_id = host_id,
        Message::SelectNotification {
            host_id,
            notification_id,
        } => {
            app.inbox_details_expanded = false;
            // Auto-mark-read on open: there is no separate "Mark read" action.
            let unread = app
                .workspace
                .notification(&host_id, &notification_id)
                .is_some_and(|record| record.status == NotificationStatus::Unread);
            if unread {
                match notification_action_task(
                    app,
                    host_id.clone(),
                    notification_id.clone(),
                    NotificationAction::Read,
                ) {
                    Ok(task) => tasks.push(task),
                    Err(err) => app.status = Some(err),
                }
            }
            app.inbox_view = InboxView::Message {
                host_id,
                notification_id,
            };
        }
        Message::InboxBack => app.inbox_view = InboxView::List,
        Message::ToggleInboxDetails => {
            app.inbox_details_expanded = !app.inbox_details_expanded;
        }
        Message::OpenNotificationLink {
            host_id,
            notification_id,
        } => {
            if app
                .workspace
                .select_notification_session(&host_id, &notification_id)
            {
                app.ui_state.selection = app.workspace.selection.clone();
                app.ui_state.active_tab = RightTab::Detail;
                app.modal = ModalView::None;
                app.inbox_view = InboxView::List;
                sync_rename_edit_for_selection(app);
                tasks.push(save_ui_state_task(app));
            } else {
                app.status = Some("linked session is no longer live".to_owned());
            }
            app.last_session_click = None;
        }
        Message::ActOnNotification {
            host_id,
            notification_id,
            action,
        } => match notification_action_task(app, host_id, notification_id, action) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::SelectSession {
            host_id,
            session_id,
        } => {
            // A second click on the already-clicked session within the window is
            // a double-click: select as usual, then open it in a terminal.
            let now = Instant::now();
            let double_click = matches!(
                &app.last_session_click,
                Some((last_host, last_session, at))
                    if *last_host == host_id
                        && *last_session == session_id
                        && now.duration_since(*at) <= DOUBLE_CLICK_WINDOW
            );
            app.workspace
                .select_session(host_id.clone(), session_id.clone());
            app.ui_state.selection = Some(Selection::Session {
                host_id: host_id.clone(),
                session_id: session_id.clone(),
            });
            // Selecting a session anywhere (tree, agents monitor) must land on
            // Detail so triage never lands behind a Linear/GitHub/Worktrees tab.
            app.ui_state.active_tab = RightTab::Detail;
            // Seed the rename buffer with the session's current name so the
            // operator edits it rather than starting from blank.
            app.rename_edit = app
                .workspace
                .hosts
                .get(&host_id)
                .and_then(|host| host.sessions.get(&session_id.0))
                .and_then(|session| session.name.clone())
                .unwrap_or_default();
            tasks.push(save_ui_state_task(app));
            if double_click {
                // Reset so a third click starts a fresh pair rather than
                // re-triggering on every subsequent click.
                app.last_session_click = None;
                match attach_task(app, &host_id, &session_id) {
                    Ok(task) => tasks.push(task),
                    Err(err) => app.status = Some(err),
                }
            } else {
                app.last_session_click = Some((host_id, session_id, now));
            }
        }
        Message::SelectProject {
            host_id,
            project_id,
        } => {
            app.workspace
                .select_project(host_id.clone(), project_id.clone());
            app.ui_state.selection = app.workspace.selection.clone();
            app.project_edit.reference = project_id;
            app.selected_action = None;
            app.start.template = None;
            // Preload the project's in-repo provider filters so the picker is
            // populated before the operator opens a provider panel.
            ensure_project_filters_loaded(app);
            // Load the project's actions eagerly so the launch action picker is
            // populated without a manual step.
            if let Ok(task) = list_project_actions_task(app) {
                tasks.push(task);
            }
            tasks.push(save_ui_state_task(app));
        }
        Message::OpenSession {
            host_id,
            session_id,
        } => match attach_task(app, &host_id, &session_id) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::OpenStartModal => {
            app.start = StartForm::default();
            app.template_recipe = None;
            app.prompt_editor = text_editor::Content::new();
            app.modal = ModalView::Start;
        }
        Message::OpenAssistantModal => {
            app.assistant = AssistantForm::default();
            app.assistant_editor = text_editor::Content::new();
            app.modal = ModalView::Assistant;
        }
        Message::CloseModal => app.modal = ModalView::None,
        Message::StartAgentSelected(agent) => app.start.agent = agent,
        Message::StartTemplateSelected(template) => {
            let chosen = (template != BLANK_TEMPLATE_LABEL).then_some(template);
            app.start.template.clone_from(&chosen);
            app.template_recipe = None;
            match chosen {
                Some(action_name) => match resolve_template_task(app, action_name) {
                    Ok(task) => tasks.push(task),
                    Err(err) => app.status = Some(err),
                },
                None => app.prompt_editor = text_editor::Content::new(),
            }
        }
        Message::TemplateResolved(result) => match result {
            Ok(resolved) => {
                app.prompt_editor = text_editor::Content::with_text(&resolved.rendered);
                app.start.agent = AgentChoice::from_wire(&resolved.recipe.agent);
                app.template_recipe = Some(resolved.recipe);
            }
            Err(err) => app.status = Some(err),
        },
        Message::PromptEdited(action) => app.prompt_editor.perform(action),
        Message::AssistantRequestEdited(action) => app.assistant_editor.perform(action),
        Message::AssistantIntentSelected(intent) => app.assistant.intent = intent,
        Message::AssistantAgentSelected(agent) => {
            app.assistant.agent = (agent != ASSISTANT_AUTO_AGENT_LABEL).then_some(agent);
        }
        Message::ToggleAssistantAdvanced => {
            app.assistant.show_advanced = !app.assistant.show_advanced;
        }
        Message::AssistantBranchChanged(value) => app.assistant.branch = value,
        Message::AssistantBaseBranchChanged(value) => app.assistant.base_branch = value,
        Message::AssistantNoSnapshotToggled(value) => app.assistant.no_snapshot = value,
        Message::AssistantDegradedToggled(value) => app.assistant.degraded = value,
        Message::ToggleStartAdvanced => app.start.show_advanced = !app.start.show_advanced,
        Message::StartBranchChanged(value) => app.start.branch = value,
        Message::StartBaseBranchChanged(value) => app.start.base_branch = value,
        Message::StartNameChanged(value) => app.start.name = value,
        Message::CreateSession => match create_session_task(app) {
            Ok(task) => {
                tasks.push(task);
                app.modal = ModalView::None;
            }
            Err(err) => app.status = Some(err),
        },
        Message::LaunchAssistant => match launch_assistant_task(app) {
            Ok(task) => {
                tasks.push(task);
                app.modal = ModalView::None;
            }
            Err(err) => app.status = Some(err),
        },
        Message::RenameEditChanged(value) => app.rename_edit = value,
        Message::RenameSession => match rename_selected_session_task(app, false) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::ClearSessionName => match rename_selected_session_task(app, true) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::OpenLinearIssue(issue_id) => {
            if let Ok(host_id) = selected_host_id(app) {
                // Stamp `active_panel` so the shared provider-item modal (which
                // disambiguates Linear vs. GitHub by that field) shows the
                // right item; the standalone Linear|GitHub toggle that used to
                // drive it is retired in favor of the top-level tab bar.
                app.workspace.apply(CoreMessage::ProviderPanelSelected {
                    host_id: host_id.clone(),
                    panel: ProviderPanel::Linear,
                });
                app.workspace
                    .apply(CoreMessage::LinearProviderIssueSelected { host_id, issue_id });
                app.modal = ModalView::ProviderItem;
            }
        }
        Message::OpenGitHubPullRequest(number) => {
            if let Ok(host_id) = selected_host_id(app) {
                app.workspace.apply(CoreMessage::ProviderPanelSelected {
                    host_id: host_id.clone(),
                    panel: ProviderPanel::GitHub,
                });
                app.workspace
                    .apply(CoreMessage::GitHubProviderPullRequestSelected { host_id, number });
                app.modal = ModalView::ProviderItem;
            }
        }
        Message::OpenGitHubIssue(number) => {
            if let Ok(host_id) = selected_host_id(app) {
                app.workspace.apply(CoreMessage::ProviderPanelSelected {
                    host_id: host_id.clone(),
                    panel: ProviderPanel::GitHub,
                });
                app.workspace
                    .apply(CoreMessage::GitHubProviderIssueSelected { host_id, number });
                app.modal = ModalView::ProviderItem;
            }
        }
        Message::InspectSelectedSession => match inspect_selected_session_task(app) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::StopSelectedSession => match stop_selected_session_task(app) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::RemoveSelectedSession => match remove_selected_session_task(app) {
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
        Message::ProjectRenameToChanged(value) => app.project_edit.rename_to = value,
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
        Message::CopyWorktreePath(path) => {
            let display = path.display().to_string();
            app.status = Some(format!("Copied path to clipboard: {display}"));
            tasks.push(clipboard_task(display));
        }
        Message::CopyText(value) => {
            app.status = Some(format!("Copied to clipboard: {value}"));
            tasks.push(clipboard_task(value));
        }
        Message::OpenUrl(url) => match open_url_task(app, url) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::RemoveWorktree(path) => match remove_worktree_task(app, path) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::SelectAction(name) => app.selected_action = Some(name),
        Message::SelectLinearFilter(name) => {
            if let Ok(host_id) = selected_host_id(app) {
                app.workspace
                    .apply(CoreMessage::LinearProviderFilterSelected { host_id, name });
            }
            // Picking a filter both selects it and fetches with it in one click.
            match begin_linear_issues_request(app) {
                Ok(request_id) => {
                    ensure_project_filters_loaded(app);
                    push_provider_task_result(
                        app,
                        &mut tasks,
                        SessionLinkProvider::Linear,
                        ProviderOperation::LinearIssues,
                        Some(request_id),
                        fetch_linear_issues_task(app, request_id),
                    );
                }
                Err(err) => app.status = Some(err),
            }
        }
        Message::SelectGitHubFilter(name) => {
            if let Ok(host_id) = selected_host_id(app) {
                app.workspace
                    .apply(CoreMessage::GitHubProviderFilterSelected { host_id, name });
            }
            // Picking a filter both selects it and fetches PRs in one click.
            match begin_github_pull_requests_request(app) {
                Ok(request_id) => {
                    ensure_project_filters_loaded(app);
                    push_provider_task_result(
                        app,
                        &mut tasks,
                        SessionLinkProvider::GitHub,
                        ProviderOperation::GitHubPullRequests,
                        Some(request_id),
                        fetch_github_pull_requests_task(app, request_id),
                    );
                }
                Err(err) => app.status = Some(err),
            }
        }
        Message::FetchLinearIssues => match begin_linear_issues_request(app) {
            Ok(request_id) => {
                ensure_project_filters_loaded(app);
                push_provider_task_result(
                    app,
                    &mut tasks,
                    SessionLinkProvider::Linear,
                    ProviderOperation::LinearIssues,
                    Some(request_id),
                    fetch_linear_issues_task(app, request_id),
                );
            }
            Err(err) => app.status = Some(err),
        },
        Message::FetchGitHubPullRequests => match begin_github_pull_requests_request(app) {
            Ok(request_id) => {
                ensure_project_filters_loaded(app);
                push_provider_task_result(
                    app,
                    &mut tasks,
                    SessionLinkProvider::GitHub,
                    ProviderOperation::GitHubPullRequests,
                    Some(request_id),
                    fetch_github_pull_requests_task(app, request_id),
                );
            }
            Err(err) => app.status = Some(err),
        },
        Message::FetchGitHubIssues => match begin_github_issues_request(app) {
            Ok(request_id) => push_provider_task_result(
                app,
                &mut tasks,
                SessionLinkProvider::GitHub,
                ProviderOperation::GitHubIssues,
                Some(request_id),
                fetch_github_issues_task(app, request_id),
            ),
            Err(err) => app.status = Some(err),
        },
        Message::FetchGitHubPullRequestStatus => {
            match begin_github_pull_request_status_request(app) {
                Ok(request_id) => push_provider_task_result(
                    app,
                    &mut tasks,
                    SessionLinkProvider::GitHub,
                    ProviderOperation::GitHubPullRequestStatus,
                    Some(request_id),
                    fetch_github_pr_status_task(app, request_id),
                ),
                Err(err) => app.status = Some(err),
            }
        }
        Message::LaunchLinearIssue => {
            push_provider_task_result(
                app,
                &mut tasks,
                SessionLinkProvider::Linear,
                ProviderOperation::Launch,
                None,
                launch_linear_issue_task(app),
            );
            app.modal = ModalView::None;
        }
        Message::LaunchGitHubPullRequest => {
            push_provider_task_result(
                app,
                &mut tasks,
                SessionLinkProvider::GitHub,
                ProviderOperation::Launch,
                None,
                launch_github_pull_request_task(app),
            );
            app.modal = ModalView::None;
        }
        Message::CoreCommandCompleted(result) => match result {
            Ok(message) => {
                // A newly created or explicitly resumed session opens straight
                // into a terminal, the same as double-clicking a live session.
                let opened_session = match &message {
                    CoreMessage::SessionCreated { host_id, session } => {
                        Some((host_id.clone(), session.id.clone()))
                    }
                    CoreMessage::SessionResumed { host_id, result } => {
                        Some((host_id.clone(), result.session.id.clone()))
                    }
                    _ => None,
                };
                // A removed session is gone from the workspace, so clear a
                // selection still pointing at it to avoid a stale detail pane.
                let removed_session = if let CoreMessage::SessionRemoveCompleted {
                    host_id,
                    session_id,
                    result,
                } = &message
                {
                    result
                        .removed
                        .then(|| (host_id.clone(), session_id.clone()))
                } else {
                    None
                };
                let deleted_notification = if let CoreMessage::NotificationDeleteCompleted {
                    host_id,
                    result,
                } = &message
                {
                    result.deleted.then(|| (host_id.clone(), result.id.clone()))
                } else {
                    None
                };
                app.workspace.apply(message);
                if let Some((host_id, session_id)) = removed_session {
                    if app.ui_state.selection
                        == Some(Selection::Session {
                            host_id,
                            session_id,
                        })
                    {
                        app.ui_state.selection = None;
                    }
                }
                if let Some((host_id, notification_id)) = deleted_notification {
                    // The deleted record's message layer is now a dead end;
                    // step back to the list instead of leaving it stranded.
                    if app.inbox_view
                        == (InboxView::Message {
                            host_id,
                            notification_id,
                        })
                    {
                        app.inbox_view = InboxView::List;
                    }
                }
                if let Some((host_id, session_id)) = opened_session {
                    match attach_task(app, &host_id, &session_id) {
                        Ok(task) => tasks.push(task),
                        Err(err) => app.status = Some(err),
                    }
                }
            }
            Err(err) => app.status = Some(err),
        },
        Message::AttachSpawned(result) => {
            app.status = Some(match result {
                Ok(()) => "attach command spawned".to_owned(),
                Err(err) => err,
            });
        }
        Message::NotificationSent(result)
        | Message::UiStateSaved(result)
        | Message::UrlOpened(result) => {
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
        Message::KeyPressed { key, modifiers } => {
            // Replays the routed message(s) through this same reducer next
            // tick, so a shortcut has no logic of its own to drift out of
            // sync with the button it stands in for.
            for message in keyboard::route_key_press(app, &key, modifiers) {
                tasks.push(Task::done(message));
            }
        }
        Message::CycleBlockedAgent => {
            // Mirrors `SelectSession`'s selection-application, minus its
            // mouse double-click bookkeeping, which has no keyboard
            // equivalent.
            let monitor = app.workspace.agent_monitor();
            if let Some((host_id, session_id)) = monitor.blocked_at(app.blocked_cycle_index) {
                app.blocked_cycle_index = app.blocked_cycle_index.wrapping_add(1);
                app.workspace
                    .select_session(host_id.clone(), session_id.clone());
                app.ui_state.selection = Some(Selection::Session {
                    host_id: host_id.clone(),
                    session_id: session_id.clone(),
                });
                app.ui_state.active_tab = RightTab::Detail;
                app.rename_edit = app
                    .workspace
                    .hosts
                    .get(&host_id)
                    .and_then(|host| host.sessions.get(&session_id.0))
                    .and_then(|session| session.name.clone())
                    .unwrap_or_default();
                tasks.push(save_ui_state_task(app));
            }
        }
    }
    Task::batch(tasks)
}

fn push_provider_task_result(
    app: &mut PohunekApp,
    tasks: &mut Vec<Task<Message>>,
    provider: SessionLinkProvider,
    operation: ProviderOperation,
    request_id: Option<ProviderRequestId>,
    result: Result<Task<Message>, String>,
) {
    match result {
        Ok(task) => tasks.push(task),
        Err(error) => match selected_host_id(app) {
            Ok(host_id) => app.workspace.apply(CoreMessage::ProviderOperationFailed {
                host_id,
                provider,
                operation,
                request_id,
                error,
            }),
            Err(_) => app.status = Some(error),
        },
    }
}

/// Writes `value` to the system clipboard; shared by every "Copy ..." action
/// (worktree paths, provider item branch names).
fn clipboard_task(value: String) -> Task<Message> {
    iced::clipboard::write::<Message>(value)
}

/// Opens `url` in the OS browser using the configured `open_url_command`,
/// argv-spawned (see `attach::spawn_open_url`) so the URL cannot inject shell
/// syntax.
fn open_url_task(app: &PohunekApp, url: String) -> Result<Task<Message>, String> {
    let command = app
        .config
        .as_ref()
        .map(|config| config.open_url_command.clone())
        .map_err(Clone::clone)?;
    Ok(Task::perform(
        async move { spawn_open_url(&command, &url) },
        Message::UrlOpened,
    ))
}

fn begin_linear_issues_request(app: &mut PohunekApp) -> Result<ProviderRequestId, String> {
    let host_id = selected_host_id(app)?;
    Ok(app.workspace.begin_linear_issues_request(host_id))
}

fn begin_github_pull_requests_request(app: &mut PohunekApp) -> Result<ProviderRequestId, String> {
    let host_id = selected_host_id(app)?;
    Ok(app.workspace.begin_github_pull_requests_request(host_id))
}

fn begin_github_issues_request(app: &mut PohunekApp) -> Result<ProviderRequestId, String> {
    let host_id = selected_host_id(app)?;
    Ok(app.workspace.begin_github_issues_request(host_id))
}

fn begin_github_pull_request_status_request(
    app: &mut PohunekApp,
) -> Result<ProviderRequestId, String> {
    let host_id = selected_host_id(app)?;
    Ok(app
        .workspace
        .begin_github_pull_request_status_request(host_id))
}

pub(crate) fn discover_hosts_task(config: &AppConfig) -> Task<Message> {
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

/// Builds and dispatches session creation from the Start modal. The input is the
/// prompt editor's text (typed for a blank session, or the edited template
/// prompt); branch/base come from the resolved template recipe when a template is
/// selected, otherwise from the Advanced overrides. The agent is the picker value.
fn create_session_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let host = selected_host_config(app)?;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    let project = selected_project_reference(app)?;
    let terminal_size = terminal_size(app)?;
    let input = app.prompt_editor.text();
    let (branch, base_branch) = match (&app.start.template, &app.template_recipe) {
        (Some(_), Some(recipe)) => (recipe.branch.clone(), recipe.base_branch.clone()),
        (Some(_), None) => return Err("the selected template is still loading".to_owned()),
        (None, _) => (
            optional_field(&app.start.branch),
            optional_field(&app.start.base_branch),
        ),
    };
    let params = SessionNewParams {
        agent: app.start.agent.as_str().to_owned(),
        name: optional_field(&app.start.name),
        cwd: None,
        cols: terminal_size.cols,
        rows: terminal_size.rows,
        project: Some(project),
        repo: None,
        branch,
        base_branch,
        input: (!input.trim().is_empty()).then_some(input),
        metadata: BTreeMap::new(),
    };
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

fn launch_assistant_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let target = selected_assistant_project(app)?;
    let host_id = target.host.id.clone();
    let options = connection_options(app)?;
    let paths = AssistantPaths::resolve().map_err(|err| err.to_string())?;
    let terminal_size = terminal_size(app)?;
    let request = optional_field(&app.assistant_editor.text());
    let params = AssistantLaunchParams {
        intent: app.assistant.intent,
        request,
        agent: app.assistant.agent.clone(),
        project: Some(target.project_ref),
        repo: None,
        branch: optional_field(&app.assistant.branch),
        base_branch: optional_field(&app.assistant.base_branch),
        cols: terminal_size.cols,
        rows: terminal_size.rows,
        no_snapshot: app.assistant.no_snapshot,
        degraded: app.assistant.degraded,
        auto_started_daemon: false,
    };
    Ok(Task::perform(
        runtime::perform(async move {
            assistant_core::launch_with_options(&target.host, &paths, params, options)
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

/// Resolves a selected template (a `None`-provider action), renders its static
/// prompt, and returns the rendered text plus recipe so the Start modal can show
/// it in an editable buffer before the operator launches the session.
fn resolve_template_task(app: &PohunekApp, action_name: String) -> Result<Task<Message>, String> {
    let host = selected_host_config(app)?;
    let options = connection_options(app)?;
    let project = selected_project_reference(app)?;
    Ok(Task::perform(
        runtime::perform(async move {
            let action = resolve_project_action_with_options(
                &host,
                ProjectActionParams {
                    reference: project,
                    name: action_name,
                },
                options,
            )
            .await
            .map_err(|err| err.to_string())?;
            let preview = preview_action_prompt(&action, String::new(), String::new())
                .map_err(|err| err.to_string())?;
            Ok(ResolvedTemplate {
                rendered: preview.rendered,
                recipe: TemplateRecipe {
                    agent: action.agent,
                    branch: action.branch,
                    base_branch: action.base_branch,
                },
            })
        }),
        Message::TemplateResolved,
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

fn remove_selected_session_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let (host, session_id) = selected_session_target(app)?;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    Ok(Task::perform(
        runtime::perform(async move {
            remove_session_with_options(&host, &session_id, options)
                .await
                .map(|result| CoreMessage::SessionRemoveCompleted {
                    host_id,
                    session_id,
                    result,
                })
                .map_err(|err| err.to_string())
        }),
        Message::CoreCommandCompleted,
    ))
}

fn notification_action_task(
    app: &PohunekApp,
    host_id: HostId,
    notification_id: NotificationId,
    action: NotificationAction,
) -> Result<Task<Message>, String> {
    let host = host_config(app, &host_id)?;
    let options = connection_options(app)?;
    Ok(match action {
        NotificationAction::Read => notification_update_task(
            host,
            host_id,
            notification_id,
            NotificationStatus::Read,
            options,
        ),
        NotificationAction::Acknowledge => notification_update_task(
            host,
            host_id,
            notification_id,
            NotificationStatus::Acknowledged,
            options,
        ),
        NotificationAction::Archive => notification_update_task(
            host,
            host_id,
            notification_id,
            NotificationStatus::Archived,
            options,
        ),
        NotificationAction::Delete => {
            notification_delete_task(host, host_id, notification_id, options)
        }
    })
}

fn notification_update_task(
    host: HostConfig,
    host_id: HostId,
    notification_id: NotificationId,
    status: NotificationStatus,
    options: ConnectionOptions,
) -> Task<Message> {
    let params = NotificationUpdateParams {
        id: notification_id,
        status,
    };
    Task::perform(
        runtime::perform(async move {
            update_notification_with_options(&host, params, options)
                .await
                .map(|result| CoreMessage::NotificationUpdateCompleted { host_id, result })
                .map_err(|err| err.to_string())
        }),
        Message::CoreCommandCompleted,
    )
}

fn notification_delete_task(
    host: HostConfig,
    host_id: HostId,
    notification_id: NotificationId,
    options: ConnectionOptions,
) -> Task<Message> {
    let params = NotificationDeleteParams {
        id: notification_id,
    };
    Task::perform(
        runtime::perform(async move {
            delete_notification_with_options(&host, params, options)
                .await
                .map(|result| CoreMessage::NotificationDeleteCompleted { host_id, result })
                .map_err(|err| err.to_string())
        }),
        Message::CoreCommandCompleted,
    )
}

pub(crate) fn resume_session_task(
    app: &PohunekApp,
    host_id: &HostId,
    session_id: &SessionId,
) -> Result<Task<Message>, String> {
    let host = host_config(app, host_id)?;
    let host_id = host_id.clone();
    let session_id = session_id.clone();
    let options = connection_options(app)?;
    Ok(Task::perform(
        runtime::perform(async move {
            resume_session_with_options(&host, &session_id, options)
                .await
                .map(|result| CoreMessage::SessionResumed { host_id, result })
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

/// Set (`clear == false`) or clear the selected session's display name. The new
/// name is the rename buffer, trimmed daemon-side; clearing ignores the buffer.
fn rename_selected_session_task(app: &PohunekApp, clear: bool) -> Result<Task<Message>, String> {
    let (host, session_id) = selected_session_target(app)?;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    let name = if clear {
        None
    } else {
        optional_field(&app.rename_edit)
    };
    let params = SessionRenameParams { session_id, name };
    Ok(Task::perform(
        runtime::perform(async move {
            rename_session_with_options(&host, params, options)
                .await
                .map(|result| CoreMessage::SessionRenamed { host_id, result })
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

fn remove_worktree_task(app: &PohunekApp, path: PathBuf) -> Result<Task<Message>, String> {
    let host = selected_host_config(app)?;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    let (_host_id, project) =
        selected_project(app).ok_or_else(|| "no project selected".to_owned())?;
    let project_id = project.id.clone();
    let params = WorktreeRemoveParams { path: path.clone() };
    Ok(Task::perform(
        runtime::perform(async move {
            remove_worktree_with_options(&host, params, options)
                .await
                .map(|result| CoreMessage::WorktreeRemoved {
                    host_id,
                    project_id,
                    path,
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
    let reference = selected_project_reference(app)?;
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

fn fetch_linear_issues_task(
    app: &PohunekApp,
    request_id: ProviderRequestId,
) -> Result<Task<Message>, String> {
    let host_id = selected_host_id(app)?;
    let config = app.config.as_ref().map_err(Clone::clone)?;
    let linear = config
        .providers
        .linear
        .clone()
        .ok_or_else(|| "Linear provider is not configured".to_owned())?;
    // The active filter's raw IssueFilter drives the query; the echoed
    // `filter_name` mirrors the host's current selection (possibly `None`) so
    // the stale-result guard rejects results after the picker changes.
    let active_filter = active_linear_filter(app);
    let host = app
        .workspace
        .hosts
        .get(&host_id)
        .ok_or_else(|| format!("unknown host `{host_id}`"))?;
    let filter_name = host.provider.linear.selected_filter.clone();
    let search = host.provider.linear.search.clone();
    let query = providers::linear::LinearQuery {
        filter: active_filter.map(|filter| filter.filter),
        search: optional_field(&search),
        ..providers::linear::LinearQuery::default()
    };
    Ok(Task::perform(
        runtime::perform(async move {
            let client = match providers::linear::HttpGraphqlTransport::try_new() {
                Ok(transport) => providers::linear::LinearClient::new(
                    providers::linear::LinearConfig {
                        token_key: linear.token_key,
                        endpoint: linear.endpoint,
                        token_lookup_timeout: linear.token_lookup_timeout,
                    },
                    providers::linear::KeyringTokenSource::new("pohunek-linear"),
                    transport,
                ),
                Err(err) => {
                    return Ok(CoreMessage::ProviderOperationFailed {
                        host_id,
                        provider: SessionLinkProvider::Linear,
                        operation: ProviderOperation::LinearIssues,
                        request_id: Some(request_id),
                        error: err.to_string(),
                    });
                }
            };
            match client.list_issues(query).await {
                Ok(issues) => Ok(CoreMessage::LinearProviderIssuesLoaded {
                    host_id,
                    request_id,
                    filter_name,
                    search,
                    issues,
                }),
                Err(err) => Ok(CoreMessage::ProviderOperationFailed {
                    host_id,
                    provider: SessionLinkProvider::Linear,
                    operation: ProviderOperation::LinearIssues,
                    request_id: Some(request_id),
                    error: err.to_string(),
                }),
            }
        }),
        Message::CoreCommandCompleted,
    ))
}

fn fetch_github_pull_requests_task(
    app: &PohunekApp,
    request_id: ProviderRequestId,
) -> Result<Task<Message>, String> {
    let (host_id, scope, client) = github_client_for_selected_project(app)?;
    let filter_args = active_github_filter(app)
        .map(|filter| filter.gh_args())
        .unwrap_or_default();
    Ok(Task::perform(
        runtime::perform(async move {
            match client.list_pull_requests(&filter_args).await {
                Ok(pull_requests) => Ok(CoreMessage::GitHubProviderPullRequestsLoaded {
                    host_id,
                    request_id,
                    scope,
                    pull_requests,
                }),
                Err(err) => Ok(CoreMessage::ProviderOperationFailed {
                    host_id,
                    provider: SessionLinkProvider::GitHub,
                    operation: ProviderOperation::GitHubPullRequests,
                    request_id: Some(request_id),
                    error: err.to_string(),
                }),
            }
        }),
        Message::CoreCommandCompleted,
    ))
}

fn fetch_github_issues_task(
    app: &PohunekApp,
    request_id: ProviderRequestId,
) -> Result<Task<Message>, String> {
    let (host_id, scope, client) = github_client_for_selected_project(app)?;
    Ok(Task::perform(
        runtime::perform(async move {
            match client.list_issues().await {
                Ok(issues) => Ok(CoreMessage::GitHubProviderIssuesLoaded {
                    host_id,
                    request_id,
                    scope,
                    issues,
                }),
                Err(err) => Ok(CoreMessage::ProviderOperationFailed {
                    host_id,
                    provider: SessionLinkProvider::GitHub,
                    operation: ProviderOperation::GitHubIssues,
                    request_id: Some(request_id),
                    error: err.to_string(),
                }),
            }
        }),
        Message::CoreCommandCompleted,
    ))
}

fn fetch_github_pr_status_task(
    app: &PohunekApp,
    request_id: ProviderRequestId,
) -> Result<Task<Message>, String> {
    let target = selected_github_pr_status_target(app)?;
    let (host_id, _scope, client) = github_client_for_selected_project(app)?;
    Ok(Task::perform(
        runtime::perform(async move {
            match client.pull_request_status(target.number).await {
                Ok(status) => Ok(CoreMessage::GitHubProviderPullRequestStatusLoaded {
                    host_id,
                    request_id,
                    status_key: target.status_key,
                    status,
                }),
                Err(err) => Ok(CoreMessage::ProviderOperationFailed {
                    host_id,
                    provider: SessionLinkProvider::GitHub,
                    operation: ProviderOperation::GitHubPullRequestStatus,
                    request_id: Some(request_id),
                    error: err.to_string(),
                }),
            }
        }),
        Message::CoreCommandCompleted,
    ))
}

fn launch_linear_issue_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let host = selected_host_config(app)?;
    let host_id = host.id.clone();
    let (project, _) = selected_project_identity(app)?;
    let action_name = launch_action_name(app, &ProviderKind::LinearIssue)?;
    let options = connection_options(app)?;
    let terminal_size = terminal_size(app)?;
    let issue = selected_linear_issue(app)?;
    let context_json = issue.to_prompt_json().to_string();
    let issue_id = issue.prompt_item_id().to_owned();
    let name = optional_field(&app.start.name);
    let item = ProviderLaunchItem::linear_issue(issue_id, context_json, issue.url)
        .map_err(|err| err.to_string())?;
    Ok(Task::perform(
        runtime::perform(async move {
            match launch_provider_item_with_options(
                &host,
                ProviderLaunchParams {
                    project,
                    action_name,
                    item,
                    cols: terminal_size.cols,
                    rows: terminal_size.rows,
                    name,
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

fn launch_github_pull_request_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let host = selected_host_config(app)?;
    let host_id = host.id.clone();
    let (project, _) = selected_project_identity(app)?;
    let action_name = launch_action_name(app, &ProviderKind::GithubPr)?;
    let options = connection_options(app)?;
    let terminal_size = terminal_size(app)?;
    let pull_request = selected_github_pull_request(app)?;
    let context_json = pull_request.to_prompt_json().to_string();
    let name = optional_field(&app.start.name);
    let item = ProviderLaunchItem::github_pull_request(
        pull_request.prompt_item_id(),
        context_json,
        pull_request.url,
    )
    .map_err(|err| err.to_string())?;
    Ok(Task::perform(
        runtime::perform(async move {
            match launch_provider_item_with_options(
                &host,
                ProviderLaunchParams {
                    project,
                    action_name,
                    item,
                    cols: terminal_size.cols,
                    rows: terminal_size.rows,
                    name,
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
