//! The Iced `update` reducer and the command/task builders it dispatches to.

use std::collections::BTreeMap;

use iced::widget::text_editor;
use iced::Task;
use pohunek_gui_core::assistant::{AssistantPaths, LaunchParams as AssistantLaunchParams};
use pohunek_gui_core::{
    assistant as assistant_core, create_session_with_options, delete_notification_with_options,
    discover_hosts, fork_session_with_options, get_notification_policy_with_options,
    inspect_session_with_options, list_project_actions_with_options, preview_action_prompt,
    read_session_output_with_options, read_session_screen_with_options,
    remove_session_with_options, rename_session_with_options, resolve_project_action_with_options,
    resume_session_with_options, set_notification_policy_with_options,
    set_session_metadata_with_options, stop_session_with_options, update_notification_with_options,
    wait_for_session_with_options, ConnectionOptions, CoreError, DomainEvent as CoreEvent,
    HostConfig, HostId, HostView, Selection, WindowSize,
};
use protocol::{
    ForkCwdMode, NotificationDeleteParams, NotificationId, NotificationPolicyParams,
    NotificationStatus, NotificationUpdateParams, ProjectActionParams, ProjectActionsParams,
    SessionForkParams, SessionId, SessionNewParams, SessionOutputParams, SessionRenameParams,
    SessionScreenParams, SessionSetMetadataParams, SessionWaitParams, MAX_SESSION_WAIT_MS,
};

use crate::attach::{attach_task, spawn_notification, window_dimension_to_u32};
use crate::config::AppConfig;
use crate::keyboard;
use crate::message::{
    AssistantForm, DiscoveryResult, InboxView, ListDirection, Message, ModalView,
    NotificationAction, ResolvedTemplate, StartForm, TemplateRecipe, ASSISTANT_AUTO_AGENT_LABEL,
    BLANK_TEMPLATE_LABEL,
};
use crate::runtime;
use crate::selection::{
    connection_options, host_config, optional_field, required_field, save_ui_state_task,
    selected_assistant_project, selected_host_config, selected_project_reference,
    selected_session_target, sync_rename_edit_for_selection, terminal_size,
};
use crate::PohunekApp;

// One GUI click reads a bounded page small enough to render responsively while
// repeated clicks continue from the headless state's exact output cursor.
const GUI_SESSION_OUTPUT_PAGE_BYTES: u32 = 16 * 1_024;

#[expect(
    clippy::too_many_lines,
    reason = "Iced update centralizes shell messages and delegates domain transitions to gui-core"
)]
pub(crate) fn update(app: &mut PohunekApp, message: Message) -> Task<Message> {
    let mut tasks = Vec::new();
    match message {
        Message::Core(event) => {
            app.workspace.apply(event);
            normalize_inbox_cursor(app);
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
        Message::OpenInbox => {
            app.modal = ModalView::Inbox;
            app.inbox_view = InboxView::List;
            app.notification_filter.host_id = None;
            app.inbox_cursor = None;
            normalize_inbox_cursor(app);
            tasks.push(keyboard::focus_task(app));
        }
        Message::OpenHostInbox(host_id) => {
            app.modal = ModalView::Inbox;
            app.inbox_view = InboxView::List;
            app.notification_filter.host_id = Some(host_id);
            app.inbox_cursor = None;
            normalize_inbox_cursor(app);
            tasks.push(keyboard::focus_task(app));
        }
        Message::SetInboxScope(scope) => {
            app.inbox_scope = scope;
            normalize_inbox_cursor(app);
        }
        Message::FilterNotificationHost(host_id) => {
            app.notification_filter.host_id = host_id;
            normalize_inbox_cursor(app);
        }
        Message::SelectNotification {
            host_id,
            notification_id,
        } => {
            app.inbox_cursor = Some((host_id.clone(), notification_id.clone()));
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
        Message::InboxBack => {
            app.inbox_view = InboxView::List;
            normalize_inbox_cursor(app);
        }
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
                app.modal = ModalView::Session;
                app.inbox_view = InboxView::List;
                sync_rename_edit_for_selection(app);
                tasks.push(save_ui_state_task(app));
            } else {
                app.status = Some("linked session is no longer live".to_owned());
            }
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
            app.workspace
                .select_session(host_id.clone(), session_id.clone());
            app.ui_state.selection = Some(Selection::Session {
                host_id: host_id.clone(),
                session_id: session_id.clone(),
            });
            // Seed the rename buffer with the session's current name so the
            // operator edits it rather than starting from blank.
            app.rename_edit = app
                .workspace
                .hosts
                .get(&host_id)
                .and_then(|host| host.sessions.get(&session_id.0))
                .and_then(|session| session.name.clone())
                .unwrap_or_default();
            app.modal = ModalView::Session;
            tasks.push(save_ui_state_task(app));
        }
        Message::SelectProject {
            host_id,
            project_id,
        } => {
            app.workspace
                .select_project(host_id.clone(), project_id.clone());
            app.ui_state.selection = app.workspace.selection.clone();
            app.start.template = None;
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
        Message::StopSession {
            host_id,
            session_id,
        } => match stop_session_task(app, &host_id, session_id) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::RequestDeleteSession {
            host_id,
            session_id,
        } => match select_session_for_delete(app, host_id, session_id) {
            Ok(()) => {
                app.modal = ModalView::ConfirmDeleteSession;
                tasks.push(save_ui_state_task(app));
            }
            Err(err) => app.status = Some(err),
        },
        Message::ConfirmDeleteSession => match delete_selected_session_task(app) {
            Ok(task) => {
                app.modal = ModalView::None;
                tasks.push(task);
            }
            Err(err) => app.status = Some(err),
        },
        Message::OpenStartModal => {
            app.start = StartForm::default();
            app.template_recipe = None;
            app.prompt_editor = text_editor::Content::new();
            app.modal = ModalView::Start;
            tasks.push(keyboard::focus_task(app));
        }
        Message::OpenAssistantModal => {
            app.assistant = AssistantForm::default();
            app.assistant_editor = text_editor::Content::new();
            app.modal = ModalView::Assistant;
            tasks.push(keyboard::focus_task(app));
        }
        Message::OpenKeymapModal => {
            app.modal = ModalView::Keymap;
            tasks.push(keyboard::focus_task(app));
        }
        Message::CloseModal => {
            app.modal = ModalView::None;
        }
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
                app.start.agent.clone_from(&resolved.recipe.agent);
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
        Message::InspectSelectedSession => match inspect_selected_session_task(app) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::ReadSelectedSessionScreen => match read_selected_session_screen_task(app) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::ReadSelectedSessionOutput => match read_selected_session_output_task(app) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::WaitForSelectedSession => match wait_for_selected_session_task(app) {
            Ok(task) => tasks.push(task),
            Err(err) => app.status = Some(err),
        },
        Message::ForkSelectedSession => match fork_selected_session_task(app) {
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
        Message::LoadNotificationPolicy(host_id) => {
            match load_notification_policy_task(app, &host_id) {
                Ok(task) => tasks.push(task),
                Err(err) => app.status = Some(err),
            }
        }
        Message::SetNotificationPolicyKind {
            host_id,
            provider,
            kind,
            enabled,
        } => {
            if !app.workspace.set_notification_policy_kind(
                &host_id,
                provider.as_deref(),
                kind,
                enabled,
            ) {
                app.status = Some("load the notification policy before editing it".to_owned());
            }
        }
        Message::SaveNotificationPolicy(host_id) => {
            match save_notification_policy_task(app, &host_id) {
                Ok(task) => tasks.push(task),
                Err(err) => app.status = Some(err),
            }
        }
        Message::MoveListSelection(direction) => move_list_selection(app, direction),
        Message::CoreCommandCompleted(result) => match result {
            Ok(message) => {
                // A newly created or explicitly resumed session opens straight
                // into a terminal, the same as double-clicking a live session.
                let opened_session = match &message {
                    CoreEvent::SessionCreated { host_id, session } => {
                        Some((host_id.clone(), session.id.clone()))
                    }
                    CoreEvent::SessionResumed { host_id, result } => {
                        Some((host_id.clone(), result.session.id.clone()))
                    }
                    CoreEvent::SessionForked { host_id, result } => {
                        Some((host_id.clone(), result.session.id.clone()))
                    }
                    _ => None,
                };
                // A removed session is gone from the workspace, so clear a
                // selection still pointing at it to avoid a stale detail pane.
                let removed_session = if let CoreEvent::SessionRemoveCompleted {
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
                let deleted_notification =
                    if let CoreEvent::NotificationDeleteCompleted { host_id, result } = &message {
                        result.deleted.then(|| (host_id.clone(), result.id.clone()))
                    } else {
                        None
                    };
                let observation_error = match &message {
                    CoreEvent::SessionObservationRuntimeChanged { error, .. } => {
                        Some(error.clone())
                    }
                    _ => None,
                };
                app.workspace.apply(message);
                if let Some(error) = observation_error {
                    app.status = Some(error);
                }
                normalize_inbox_cursor(app);
                if let Some((host_id, session_id)) = removed_session {
                    if app.ui_state.selection
                        == Some(Selection::Session {
                            host_id,
                            session_id,
                        })
                    {
                        app.ui_state.selection = None;
                        tasks.push(save_ui_state_task(app));
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
        Message::KeyPressed { key, modifiers } => {
            // Replays the routed message(s) through this same reducer next
            // tick, so a shortcut has no logic of its own to drift out of
            // sync with the button it stands in for.
            for message in keyboard::route_key_press(app, &key, modifiers) {
                tasks.push(Task::done(message));
            }
        }
    }
    Task::batch(tasks)
}

fn move_list_selection(app: &mut PohunekApp, direction: ListDirection) {
    if app.modal == ModalView::Inbox && matches!(app.inbox_view, InboxView::List) {
        move_inbox_cursor(app, direction);
    }
}

fn move_inbox_cursor(app: &mut PohunekApp, direction: ListDirection) {
    let rows = app
        .workspace
        .inbox_rows(app.inbox_scope, &app.notification_filter);
    if rows.is_empty() {
        app.inbox_cursor = None;
        return;
    }
    let current_index = app.inbox_cursor.as_ref().and_then(|(host_id, id)| {
        rows.iter()
            .position(|row| &row.host_id == host_id && &row.record.id == id)
    });
    let selected_index = match (current_index, direction) {
        (None, ListDirection::Down) => 0,
        (None | Some(0), ListDirection::Up) => rows.len() - 1,
        (Some(index), ListDirection::Down) => (index + 1) % rows.len(),
        (Some(index), ListDirection::Up) => index - 1,
    };
    let row = &rows[selected_index];
    app.inbox_cursor = Some((row.host_id.clone(), row.record.id.clone()));
}

fn normalize_inbox_cursor(app: &mut PohunekApp) {
    if app.modal != ModalView::Inbox || !matches!(app.inbox_view, InboxView::List) {
        return;
    }
    let rows = app
        .workspace
        .inbox_rows(app.inbox_scope, &app.notification_filter);
    if rows.is_empty() {
        app.inbox_cursor = None;
        return;
    }
    let cursor_is_visible = app.inbox_cursor.as_ref().is_some_and(|(host_id, id)| {
        rows.iter()
            .any(|row| &row.host_id == host_id && &row.record.id == id)
    });
    if !cursor_is_visible {
        let row = &rows[0];
        app.inbox_cursor = Some((row.host_id.clone(), row.record.id.clone()));
    }
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
    let host_view = app
        .workspace
        .hosts
        .get(&host_id)
        .ok_or_else(|| format!("unknown host `{host_id}`"))?;
    ensure_agent_launchable(&host_id, host_view, &app.start.agent)?;
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
        agent: app.start.agent.clone(),
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
                .map(|result| CoreEvent::SessionCreated {
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
    let host = app
        .workspace
        .hosts
        .get(&host_id)
        .ok_or_else(|| format!("unknown host `{host_id}`"))?;
    ensure_assistant_agent_launchable(&host_id, host, app.assistant.agent.as_deref())?;
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
                .map(|result| CoreEvent::SessionCreated {
                    host_id,
                    session: result.session,
                })
                .map_err(|err| err.to_string())
        }),
        Message::CoreCommandCompleted,
    ))
}

fn ensure_agent_launchable(host_id: &HostId, host: &HostView, agent: &str) -> Result<(), String> {
    if host.agent_is_launchable(agent) {
        Ok(())
    } else {
        Err(format!(
            "agent runtime `{agent}` is not launchable on host `{host_id}`"
        ))
    }
}

fn ensure_assistant_agent_launchable(
    host_id: &HostId,
    host: &HostView,
    agent: Option<&str>,
) -> Result<(), String> {
    match agent {
        Some(agent) if host.agent_is_assistant_capable(agent) => Ok(()),
        Some(agent) => Err(format!(
            "agent runtime `{agent}` cannot host the assistant on host `{host_id}`"
        )),
        None if !host.launchable_assistant_agents().is_empty() => Ok(()),
        None => Err(format!(
            "host `{host_id}` has no launchable assistant runtime"
        )),
    }
}

fn inspect_selected_session_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let (host, session_id) = selected_session_target(app)?;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    Ok(Task::perform(
        runtime::perform(async move {
            inspect_session_with_options(&host, &session_id, options)
                .await
                .map(|session| CoreEvent::SessionInspected { host_id, session })
                .map_err(|err| err.to_string())
        }),
        Message::CoreCommandCompleted,
    ))
}

fn read_selected_session_screen_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let (host, session_id) = selected_session_target(app)?;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    let params = SessionScreenParams::new(session_id);
    Ok(Task::perform(
        runtime::perform(async move {
            read_session_screen_with_options(&host, params, options)
                .await
                .map(|result| CoreEvent::SessionScreenLoaded { host_id, result })
                .map_err(|err| err.to_string())
        }),
        Message::CoreCommandCompleted,
    ))
}

fn read_selected_session_output_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let (host, session_id) = selected_session_target(app)?;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    let observed_session_id = session_id.clone();
    let params = session_output_params(
        session_id,
        app.workspace
            .session_observation(&host_id, &observed_session_id),
    )?;
    Ok(Task::perform(
        runtime::perform(async move {
            map_session_output_result(
                host_id,
                observed_session_id,
                read_session_output_with_options(&host, params, options).await,
            )
        }),
        Message::CoreCommandCompleted,
    ))
}

fn session_output_params(
    session_id: SessionId,
    observation: Option<&pohunek_gui_core::SessionObservation>,
) -> Result<SessionOutputParams, String> {
    let runtime = observation.and_then(|observation| observation.output_runtime.clone());
    let cursor = observation.and_then(|observation| observation.output_cursor);
    SessionOutputParams::new(
        session_id,
        runtime,
        cursor,
        GUI_SESSION_OUTPUT_PAGE_BYTES,
        None,
    )
    .map_err(|error| error.to_string())
}

fn map_session_output_result(
    host_id: HostId,
    session_id: SessionId,
    result: Result<protocol::SessionOutputResult, CoreError>,
) -> Result<CoreEvent, String> {
    match result {
        Ok(result) => Ok(CoreEvent::SessionOutputLoaded { host_id, result }),
        Err(error) if error.is_session_runtime_changed() => {
            Ok(CoreEvent::SessionObservationRuntimeChanged {
                host_id,
                session_id,
                error: error.to_string(),
            })
        }
        Err(error) => Err(error.to_string()),
    }
}

fn wait_for_selected_session_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let (host, session_id) = selected_session_target(app)?;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    let session = app
        .workspace
        .hosts
        .get(&host_id)
        .and_then(|host| host.sessions.get(&session_id.0))
        .ok_or_else(|| "selected session is not loaded".to_owned())?;
    let runtime = session.runtime.as_ref().and_then(|runtime| {
        runtime.runtime_id.as_ref().and_then(|runtime_id| {
            protocol::SessionRuntimeIdentity::new(runtime_id.clone(), runtime.runtime_generation)
                .ok()
        })
    });
    let params = SessionWaitParams::new(
        session_id,
        runtime,
        Some(session.updated_at.clone()),
        None,
        None,
        None,
        None,
        MAX_SESSION_WAIT_MS,
    )
    .map_err(|error| error.to_string())?;
    Ok(Task::perform(
        runtime::perform(async move {
            wait_for_session_with_options(&host, params, options)
                .await
                .map(|result| CoreEvent::SessionWaitCompleted { host_id, result })
                .map_err(|err| err.to_string())
        }),
        Message::CoreCommandCompleted,
    ))
}

fn load_notification_policy_task(
    app: &PohunekApp,
    host_id: &HostId,
) -> Result<Task<Message>, String> {
    let host = host_config(app, host_id)?;
    let host_id = host_id.clone();
    let options = connection_options(app)?;
    Ok(Task::perform(
        runtime::perform(async move {
            get_notification_policy_with_options(&host, options)
                .await
                .map(|result| CoreEvent::NotificationPolicyLoaded { host_id, result })
                .map_err(|err| err.to_string())
        }),
        Message::CoreCommandCompleted,
    ))
}

fn save_notification_policy_task(
    app: &PohunekApp,
    host_id: &HostId,
) -> Result<Task<Message>, String> {
    let host = host_config(app, host_id)?;
    let policy = app
        .workspace
        .notification_policy(host_id)
        .cloned()
        .ok_or_else(|| "load the notification policy before saving it".to_owned())?;
    let host_id = host_id.clone();
    let options = connection_options(app)?;
    Ok(Task::perform(
        runtime::perform(async move {
            set_notification_policy_with_options(
                &host,
                NotificationPolicyParams { policy },
                options,
            )
            .await
            .map(|result| CoreEvent::NotificationPolicyLoaded { host_id, result })
            .map_err(|err| err.to_string())
        }),
        Message::CoreCommandCompleted,
    ))
}

fn fork_selected_session_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let (host, session_id) = selected_session_target(app)?;
    fork_session_task(app, &host.id, &session_id)
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

fn stop_session_task(
    app: &PohunekApp,
    host_id: &HostId,
    session_id: SessionId,
) -> Result<Task<Message>, String> {
    let row = app
        .workspace
        .session_rows()
        .into_iter()
        .find(|row| row.host_id == *host_id && row.session_id == session_id)
        .ok_or_else(|| "session is no longer loaded".to_owned())?;
    if !row.can_stop {
        return Err("session cannot be stopped safely in its current state".to_owned());
    }
    let host = host_config(app, host_id)?;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    Ok(Task::perform(
        runtime::perform(async move {
            stop_session_with_options(&host, &session_id, options)
                .await
                .map(|result| CoreEvent::SessionStopCompleted {
                    host_id,
                    session_id,
                    result,
                })
                .map_err(|err| err.to_string())
        }),
        Message::CoreCommandCompleted,
    ))
}

fn select_session_for_delete(
    app: &mut PohunekApp,
    host_id: HostId,
    session_id: SessionId,
) -> Result<(), String> {
    let row = app
        .workspace
        .session_rows()
        .into_iter()
        .find(|row| row.host_id == host_id && row.session_id == session_id)
        .ok_or_else(|| "session is no longer loaded".to_owned())?;
    if !row.can_remove {
        return Err("session cannot be deleted safely in its current state".to_owned());
    }
    app.workspace
        .select_session(host_id.clone(), session_id.clone());
    app.ui_state.selection = Some(Selection::Session {
        host_id,
        session_id,
    });
    Ok(())
}

fn delete_selected_session_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let (host, session_id) = selected_session_target(app)?;
    let host_id = host.id.clone();
    let row = app
        .workspace
        .session_rows()
        .into_iter()
        .find(|row| row.host_id == host_id && row.session_id == session_id)
        .ok_or_else(|| "session is no longer loaded".to_owned())?;
    if !row.can_remove {
        return Err("session cannot be deleted safely in its current state".to_owned());
    }
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    Ok(Task::perform(
        runtime::perform(async move {
            remove_session_with_options(&host, &session_id, options)
                .await
                .map(|result| CoreEvent::SessionRemoveCompleted {
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
                .map(|result| CoreEvent::NotificationUpdateCompleted { host_id, result })
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
                .map(|result| CoreEvent::NotificationDeleteCompleted { host_id, result })
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
                .map(|result| CoreEvent::SessionResumed { host_id, result })
                .map_err(|err| err.to_string())
        }),
        Message::CoreCommandCompleted,
    ))
}

pub(crate) fn fork_session_task(
    app: &PohunekApp,
    host_id: &HostId,
    session_id: &SessionId,
) -> Result<Task<Message>, String> {
    let can_fork = app
        .workspace
        .hosts
        .get(host_id)
        .and_then(|host| host.sessions.get(&session_id.0))
        .is_some_and(|session| session.capabilities.fork);
    if !can_fork {
        return Err("session does not support fork".to_owned());
    }
    let host = host_config(app, host_id)?;
    let host_id = host_id.clone();
    let session_id = session_id.clone();
    let options = connection_options(app)?;
    let terminal_size = terminal_size(app)?;
    let params = SessionForkParams {
        session_id,
        name: None,
        cwd_mode: ForkCwdMode::Same,
        cols: terminal_size.cols,
        rows: terminal_size.rows,
    };
    Ok(Task::perform(
        runtime::perform(async move {
            fork_session_with_options(&host, params, options)
                .await
                .map(|result| CoreEvent::SessionForked { host_id, result })
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
                .map(|result| CoreEvent::SessionMetadataUpdated { host_id, result })
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
                .map(|result| CoreEvent::SessionRenamed { host_id, result })
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
                Ok(result) => Ok(CoreEvent::ProjectActionsLoaded {
                    host_id,
                    reference,
                    result,
                }),
                Err(err) => Ok(CoreEvent::HostOperationFailed {
                    host_id,
                    error: err.to_string(),
                }),
            }
        }),
        Message::CoreCommandCompleted,
    ))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use pohunek_gui_core::{
        ConnState, NotificationFilter, NotificationScope, PromptState, ProviderState, UiState,
        Workspace,
    };
    use protocol::{
        AgentKind, AgentRuntime, NotificationKind, NotificationRecord, NotificationSeverity,
        NotificationSource, ProjectInfo, ProjectSource,
    };

    use super::*;
    use crate::message::MetadataEdit;

    #[test]
    fn launch_guard_uses_runtime_capabilities_and_preserves_legacy_profiles() {
        let host_id = HostId::new("local");
        let mut host = test_host();
        host.runtimes = vec![
            test_runtime("legacy-custom", None, true, None),
            test_runtime("hermes", None, true, None),
            test_runtime(
                "hermes-supported",
                Some(AgentKind::Hermes),
                true,
                Some(true),
            ),
            test_runtime(
                "future-profile",
                Some(AgentKind::Unknown("future".to_owned())),
                true,
                Some(true),
            ),
        ];

        ensure_agent_launchable(&host_id, &host, "legacy-custom")
            .expect("legacy custom runtime remains launchable");
        ensure_agent_launchable(&host_id, &host, "hermes-supported")
            .expect("supported Hermes runtime is launchable");
        assert!(ensure_agent_launchable(&host_id, &host, "hermes").is_err());
        assert!(ensure_agent_launchable(&host_id, &host, "future-profile").is_err());
        assert!(ensure_agent_launchable(&host_id, &host, "missing-profile").is_err());
    }

    #[test]
    fn assistant_launch_guard_rejects_shell_backed_profiles() {
        let host_id = HostId::new("local");
        let mut host = test_host();
        host.runtimes = vec![
            test_runtime("shell-profile", Some(AgentKind::Shell), true, None),
            test_runtime("legacy-custom", None, true, None),
        ];

        assert!(ensure_assistant_agent_launchable(&host_id, &host, Some("shell-profile")).is_err());
        ensure_assistant_agent_launchable(&host_id, &host, Some("legacy-custom"))
            .expect("legacy custom runtime can host the assistant");
        ensure_assistant_agent_launchable(&host_id, &host, None)
            .expect("auto selection can use the legacy custom runtime");
    }

    #[test]
    fn move_list_selection_moves_inbox_cursor_with_wrapping() {
        let host_id = HostId::new("local");
        let first_id = NotificationId("n-1".to_owned());
        let second_id = NotificationId("n-2".to_owned());
        let mut host = test_host();
        host.notifications.insert(
            first_id.0.clone(),
            test_notification(&first_id, "2026-07-06T00:00:00Z"),
        );
        host.notifications.insert(
            second_id.0.clone(),
            test_notification(&second_id, "2026-07-05T00:00:00Z"),
        );
        let mut app = app_without_selection();
        app.workspace.hosts.insert(host_id.clone(), host);
        app.modal = ModalView::Inbox;
        app.inbox_view = InboxView::List;

        let _ = update(&mut app, Message::MoveListSelection(ListDirection::Down));
        assert_eq!(app.inbox_cursor, Some((host_id.clone(), first_id.clone())));

        let _ = update(&mut app, Message::MoveListSelection(ListDirection::Down));
        assert_eq!(app.inbox_cursor, Some((host_id.clone(), second_id.clone())));

        let _ = update(&mut app, Message::MoveListSelection(ListDirection::Down));
        assert_eq!(app.inbox_cursor, Some((host_id, first_id)));
    }

    #[test]
    fn runtime_changed_output_error_invalidates_the_cached_cursor() {
        let host_id = HostId::new("local");
        let session_id = SessionId("s-1".to_owned());
        let runtime =
            protocol::SessionRuntimeIdentity::new("runtime-1", protocol::RuntimeGeneration::new(1))
                .expect("valid runtime identity");
        let mut workspace = Workspace::default();
        workspace.apply(CoreEvent::SessionOutputLoaded {
            host_id: host_id.clone(),
            result: protocol::SessionOutputResult::new(
                session_id.clone(),
                runtime,
                protocol::OutputOffset::new(0),
                protocol::OutputOffset::new(0),
                protocol::OutputOffset::new(1),
                protocol::OutputOffset::new(1),
                "YQ==",
                None,
                false,
                false,
            )
            .expect("valid output result"),
        });
        let event = map_session_output_result(
            host_id.clone(),
            session_id.clone(),
            Err(CoreError::Protocol(
                protocol::ProtocolError::session_runtime_changed(),
            )),
        )
        .expect("runtime change becomes a reducible event");

        assert!(matches!(
            &event,
            CoreEvent::SessionObservationRuntimeChanged {
                host_id: actual_host,
                session_id: actual_session,
                ..
            } if actual_host == &host_id && actual_session == &session_id
        ));
        workspace.apply(event);

        let retry = session_output_params(
            session_id.clone(),
            workspace.session_observation(&host_id, &session_id),
        )
        .expect("retry params");
        assert!(retry.runtime().is_none());
        assert!(retry.after_offset().is_none());
    }

    fn app_without_selection() -> PohunekApp {
        PohunekApp {
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
            notification_filter: NotificationFilter::default(),
            inbox_scope: NotificationScope::default(),
            inbox_view: InboxView::default(),
            inbox_cursor: None,
            inbox_details_expanded: false,
            metadata_edit: MetadataEdit::default(),
            rename_edit: String::new(),
            state_dir: None,
            status: None,
            notified_intents: 0,
        }
    }

    fn test_host() -> HostView {
        HostView {
            conn: ConnState::Connected,
            health: None,
            sessions: BTreeMap::new(),
            projects: BTreeMap::from([("p-1".to_owned(), test_project())]),
            project_details: BTreeMap::new(),
            notifications: BTreeMap::new(),
            prompt: PromptState::default(),
            provider: ProviderState::default(),
            review: pohunek_gui_core::ReviewTabState::default(),
            last_agent_state: None,
            last_error: None,
            supported_agents: Vec::new(),
            runtimes: Vec::new(),
            notification_providers: Vec::new(),
            observation_capabilities: pohunek_gui_core::ObservationCapabilities::default(),
        }
    }

    fn test_runtime(
        name: &str,
        agent_base: Option<AgentKind>,
        available: bool,
        supported: Option<bool>,
    ) -> AgentRuntime {
        AgentRuntime {
            agent: name.to_owned(),
            agent_base,
            available,
            path: None,
            version: None,
            supported,
        }
    }

    fn test_project() -> ProjectInfo {
        ProjectInfo {
            id: "p-1".to_owned(),
            label: "Project".to_owned(),
            repo_root: PathBuf::from("/tmp/project"),
            git_common_dir: PathBuf::from("/tmp/project/.git"),
            origin_url: None,
            default_base_branch: None,
            source: ProjectSource::Manual,
            is_bare: false,
            added_at: "2026-07-06T00:00:00Z".to_owned(),
            last_used_at: "2026-07-06T00:00:00Z".to_owned(),
        }
    }

    fn test_notification(id: &NotificationId, created_at: &str) -> NotificationRecord {
        NotificationRecord {
            id: id.clone(),
            source: NotificationSource {
                provider: "test".to_owned(),
                provider_event: "event".to_owned(),
                host_local_source_id: "source-1".to_owned(),
            },
            kind: NotificationKind::AgentBlocked,
            severity: NotificationSeverity::Warning,
            status: NotificationStatus::Unread,
            title: "Blocked".to_owned(),
            body: "Needs attention".to_owned(),
            metadata: BTreeMap::new(),
            created_at: created_at.to_owned(),
            session_id: None,
            agent_kind: Some(AgentKind::Codex),
            source_id: None,
            dedupe_key: None,
            project_id: Some("p-1".to_owned()),
            read_at: None,
            acked_at: None,
            archived_at: None,
            deleted_at: None,
            superseded_by: None,
        }
    }
}
