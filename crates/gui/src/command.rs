//! The Iced `update` reducer and the command/task builders it dispatches to.

use std::collections::BTreeMap;

use iced::widget::text_editor;
use iced::Task;
use pohunek_gui_core::assistant::{AssistantPaths, LaunchParams as AssistantLaunchParams};
use pohunek_gui_core::{
    assistant as assistant_core, create_session_with_options, delete_notification_with_options,
    discover_hosts, fork_session_with_options, get_notification_policy_with_options,
    inspect_host_governance_with_options, inspect_session_with_options,
    list_project_actions_with_options, preview_action_prompt, read_session_output_with_options,
    read_session_screen_with_options, remove_session_with_options, rename_session_with_options,
    resolve_project_action_with_options, resume_session_with_options,
    set_notification_policy_with_options, set_session_metadata_with_options,
    stop_session_with_options, update_notification_with_options, wait_for_session_with_options,
    ConnectionOptions, CoreError, DomainEvent as CoreEvent, HostConfig, HostId, HostView,
    ProjectRef, Selection, WindowSize,
};
use protocol::{
    ForkCwdMode, NotificationDeleteParams, NotificationId, NotificationPolicyParams,
    NotificationStatus, NotificationUpdateParams, ProjectActionParams, ProjectActionsParams,
    SessionForkParams, SessionId, SessionNewParams, SessionOutputParams, SessionRenameParams,
    SessionScreenParams, SessionSetMetadataParams, SessionWaitParams, MAX_SESSION_WAIT_MS,
};

use crate::attach::{attach_task, window_dimension_to_u32};
use crate::config::AppConfig;
use crate::keyboard;
use crate::message::{
    AssistantForm, DiscoveryResult, FormField, FormSelect, InboxView, ListDirection, Message,
    ModalView, NotificationAction, ResolvedTemplate, StartForm, TemplateRecipe,
    ASSISTANT_AUTO_AGENT_LABEL, BLANK_TEMPLATE_LABEL,
};
use crate::notify::{apply_outcome, NotificationOutcome};
use crate::runtime;
use crate::selection::{
    connection_options, host_config, optional_field, preselected_project, project_host,
    project_target, required_field, save_ui_state_task, selected_session_target,
    sync_rename_edit_for_selection, terminal_size,
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
            let governance_host = match &event {
                CoreEvent::HostSnapshotLoaded { snapshot } => Some(snapshot.host_id.clone()),
                _ => None,
            };
            app.workspace.apply(event);
            normalize_project_filter(app);
            tasks.extend(normalize_launch_forms(app));
            if let Some(host_id) = governance_host {
                match governance_inspect_task(app, host_id) {
                    Ok(task) => tasks.push(task),
                    Err(err) => app.status = Some(err),
                }
            }
            normalize_inbox_cursor(app);
            tasks.push(notification_tasks(app));
        }
        Message::HostsDiscovered(result) => {
            app.hosts = result.hosts;
            app.status = result.warning;
        }
        Message::SetProjectFilter(filter) => {
            app.project_filter = filter;
            if drop_selection_outside_filter(app) {
                tasks.push(save_ui_state_task(app));
            }
        }
        Message::OpenHostsModal => {
            app.modal = ModalView::Hosts;
            tasks.push(keyboard::focus_task(app));
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
            open_start_modal(app);
            tasks.extend(load_start_actions_task(app));
            tasks.push(keyboard::focus_task(app));
        }
        Message::OpenAssistantModal => {
            app.assistant = AssistantForm {
                project: preselected_project(app),
                ..AssistantForm::default()
            };
            ensure_assistant_agent_matches_host(app);
            app.assistant_editor = text_editor::Content::new();
            app.form_focus = FormField::AssistantProject;
            app.form_select = None;
            app.modal = ModalView::Assistant;
            tasks.push(keyboard::focus_task(app));
        }
        Message::OpenKeymapModal => {
            app.modal = ModalView::Keymap;
            tasks.push(keyboard::focus_task(app));
        }
        Message::CloseModal => {
            app.modal = ModalView::None;
            app.form_select = None;
        }
        Message::StartProjectSelected(project) => {
            if app.start.project.as_ref() != Some(&project) {
                app.template_generation += 1;
                if app.start.template.take().is_some() {
                    app.prompt_editor = text_editor::Content::new();
                }
                app.template_recipe = None;
                app.start.project = Some(project);
                ensure_start_agent_matches_host(app);
                tasks.extend(load_start_actions_task(app));
            }
        }
        Message::AssistantProjectSelected(project) => {
            if app.assistant.project.as_ref() != Some(&project) {
                app.assistant.project = Some(project);
                ensure_assistant_agent_matches_host(app);
            }
        }
        Message::StartAgentSelected(agent) => app.start.agent = agent,
        Message::StartTemplateSelected(template) => {
            app.template_generation += 1;
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
        Message::TemplateResolved { generation, result } => {
            if generation == app.template_generation {
                match result {
                    Ok(resolved) => {
                        app.prompt_editor = text_editor::Content::with_text(&resolved.rendered);
                        app.start.agent.clone_from(&resolved.recipe.agent);
                        app.template_recipe = Some(resolved.recipe);
                    }
                    Err(err) => app.status = Some(err),
                }
            }
        }
        Message::PromptEdited(action) => {
            app.form_focus = FormField::StartPrompt;
            app.prompt_editor.perform(action);
        }
        Message::AssistantRequestEdited(action) => {
            app.form_focus = FormField::AssistantRequest;
            app.assistant_editor.perform(action);
        }
        Message::AssistantIntentSelected(intent) => app.assistant.intent = intent,
        Message::AssistantAgentSelected(agent) => {
            app.assistant.agent = (agent != ASSISTANT_AUTO_AGENT_LABEL).then_some(agent);
        }
        Message::ToggleAssistantAdvanced => {
            app.assistant.show_advanced = !app.assistant.show_advanced;
        }
        Message::AssistantBranchChanged(value) => {
            app.form_focus = FormField::AssistantBranch;
            app.assistant.branch = value;
        }
        Message::AssistantBaseBranchChanged(value) => {
            app.form_focus = FormField::AssistantBaseBranch;
            app.assistant.base_branch = value;
        }
        Message::AssistantNoSnapshotToggled(value) => app.assistant.no_snapshot = value,
        Message::AssistantDegradedToggled(value) => app.assistant.degraded = value,
        Message::ToggleFormSelect(field) => {
            if keyboard::form_field_is_visible(app, field) {
                app.form_focus = field;
                if app.form_select.is_some_and(|select| select.field == field) {
                    app.form_select = None;
                } else {
                    let options = keyboard::form_select_options(app, field);
                    if !options.is_empty() {
                        app.form_select = Some(FormSelect {
                            field,
                            cursor: keyboard::form_select_cursor(app, field),
                        });
                    }
                }
                tasks.push(keyboard::form_field_focus_task(field));
            }
        }
        Message::MoveFormSelect(direction) => {
            move_form_select(app, direction);
        }
        Message::ConfirmFormSelect => {
            if let Some(select) = app.form_select.take() {
                if let Some(message) = keyboard::form_select_choice_message(app, select) {
                    tasks.push(Task::done(message));
                }
            }
        }
        Message::CloseFormSelect => app.form_select = None,
        Message::ChooseFormSelect { field, index } => {
            if keyboard::form_field_is_visible(app, field) {
                app.form_focus = field;
                app.form_select = None;
                if let Some(message) = keyboard::form_select_choice_message(
                    app,
                    FormSelect {
                        field,
                        cursor: index,
                    },
                ) {
                    tasks.push(Task::done(message));
                }
                tasks.push(keyboard::form_field_focus_task(field));
            }
        }
        Message::TraverseFormFocus { focused, direction } => {
            let target = keyboard::next_form_field(app, focused, direction);
            app.form_focus = target;
            app.form_select = None;
            tasks.push(keyboard::form_field_focus_task(target));
        }
        Message::ToggleStartAdvanced => app.start.show_advanced = !app.start.show_advanced,
        Message::StartBranchChanged(value) => {
            app.form_focus = FormField::StartBranch;
            app.start.branch = value;
        }
        Message::StartBaseBranchChanged(value) => {
            app.form_focus = FormField::StartBaseBranch;
            app.start.base_branch = value;
        }
        Message::StartNameChanged(value) => {
            app.form_focus = FormField::StartName;
            app.start.name = value;
        }
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
                normalize_project_filter(app);
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
                Ok(None) => "attach command spawned".to_owned(),
                Ok(Some(warning)) => format!("attach command spawned (warning: {warning})"),
                Err(err) => err,
            });
        }
        Message::NotificationSent(outcome) => {
            if let Some(status) = apply_outcome(&mut app.notification_health, &outcome) {
                app.status = Some(status);
            }
        }
        Message::UiStateSaved(result) => {
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
            if let Some(message) = keyboard::form_submit_message(app, &key, modifiers) {
                tasks.push(Task::done(message));
            } else if let Some(message) = keyboard::form_select_key_message(app, &key, modifiers) {
                tasks.push(Task::done(message));
            } else if let Some(task) = keyboard::form_focus_task(app, &key, modifiers) {
                tasks.push(task);
            } else if keyboard::form_reserves_enter(app, &key) {
                // Bare Enter confirms an expanded select, but never submits a
                // launch form. Ctrl+Enter is the only form submit chord.
            } else {
                // Replays the routed message(s) through this same reducer next
                // tick, so a shortcut has no logic of its own to drift out of
                // sync with the button it stands in for.
                for message in keyboard::route_key_press(app, &key, modifiers) {
                    tasks.push(Task::done(message));
                }
            }
        }
    }
    Task::batch(tasks)
}

/// Drops a project filter whose project no longer has any session, so the list
/// never stays filtered to an empty view with no chip to clear it.
fn normalize_project_filter(app: &mut PohunekApp) {
    let Some(filter) = &app.project_filter else {
        return;
    };
    let still_listed = app
        .workspace
        .session_project_filters()
        .iter()
        .any(|choice| &choice.project == filter);
    if !still_listed {
        app.project_filter = None;
    }
}

/// Replaces a launch form's project that its host no longer lists with the
/// preselection rule's project, or clears it when that yields none. The typed
/// prompt, request and name text stay. Returns the action reload for the Start
/// form when it gained a new project while its modal is open.
fn normalize_launch_forms(app: &mut PohunekApp) -> Option<Task<Message>> {
    let mut start_project_changed = false;
    if let Some(project) = app.start.project.clone() {
        if project_host(app, Some(&project)).is_none() {
            app.template_generation += 1;
            app.start.template = None;
            app.template_recipe = None;
            app.start.project = preselected_project(app);
            ensure_start_agent_matches_host(app);
            start_project_changed = true;
        }
    }
    if let Some(project) = app.assistant.project.clone() {
        if project_host(app, Some(&project)).is_none() {
            app.assistant.project = preselected_project(app);
            ensure_assistant_agent_matches_host(app);
        }
    }
    if start_project_changed && app.modal == ModalView::Start {
        load_start_actions_task(app)
    } else {
        None
    }
}

/// Clears the session selection when the active project filter hides the
/// selected session, so global session keys never act on an unlisted row.
/// Returns whether the selection changed.
fn drop_selection_outside_filter(app: &mut PohunekApp) -> bool {
    let Some(Selection::Session {
        host_id,
        session_id,
    }) = &app.ui_state.selection
    else {
        return false;
    };
    let listed = app
        .workspace
        .session_rows_filtered(app.project_filter.as_ref())
        .iter()
        .any(|row| row.host_id == *host_id && row.session_id == *session_id);
    if listed {
        return false;
    }
    app.ui_state.selection = None;
    app.workspace.selection = None;
    true
}

fn open_start_modal(app: &mut PohunekApp) {
    app.template_generation += 1;
    app.start = StartForm {
        project: preselected_project(app),
        ..StartForm::default()
    };
    ensure_start_agent_matches_host(app);
    app.form_focus = FormField::StartProject;
    app.form_select = None;
    app.template_recipe = None;
    app.prompt_editor = text_editor::Content::new();
    app.modal = ModalView::Start;
}

/// Keeps the Start form's agent launchable on the project's host: when the
/// current agent is not, falls back to the host's first launchable agent.
fn ensure_start_agent_matches_host(app: &mut PohunekApp) {
    let Some(host) = project_host(app, app.start.project.as_ref()) else {
        return;
    };
    if host.agent_is_launchable(&app.start.agent) {
        return;
    }
    if let Some(agent) = host.launchable_agents().into_iter().next() {
        app.start.agent = agent;
    }
}

/// Resets an explicit assistant agent to automatic selection when the project's
/// host cannot host the assistant with it.
fn ensure_assistant_agent_matches_host(app: &mut PohunekApp) {
    let Some(agent) = app.assistant.agent.as_deref() else {
        return;
    };
    let capable = project_host(app, app.assistant.project.as_ref())
        .is_some_and(|host| host.agent_is_assistant_capable(agent));
    if !capable {
        app.assistant.agent = None;
    }
}

/// Loads the Start form's project actions so the template picker is populated
/// without a manual step.
fn load_start_actions_task(app: &PohunekApp) -> Option<Task<Message>> {
    let project = app.start.project.as_ref()?;
    list_project_actions_task(app, project).ok()
}

fn move_form_select(app: &mut PohunekApp, direction: ListDirection) {
    let Some(mut select) = app.form_select else {
        return;
    };
    let option_count = keyboard::form_select_options(app, select.field).len();
    if option_count == 0 {
        app.form_select = None;
        return;
    }
    let cursor = select.cursor.min(option_count - 1);
    select.cursor = match direction {
        ListDirection::Up if cursor == 0 => option_count - 1,
        ListDirection::Up => cursor - 1,
        ListDirection::Down => (cursor + 1) % option_count,
    };
    app.form_select = Some(select);
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
        let notifier = config.notification.clone();
        Task::perform(
            runtime::perform_blocking_or(
                move || notifier.notify(&intent.title, &intent.body),
                NotificationOutcome::Unavailable,
            ),
            Message::NotificationSent,
        )
    }))
}

/// Request safe governance data after a host snapshot establishes the route.
fn governance_inspect_task(app: &mut PohunekApp, host_id: HostId) -> Result<Task<Message>, String> {
    let host = host_config(app, &host_id)?;
    let options = connection_options(app)?;
    let request_id = app
        .workspace
        .begin_governance_request(host_id.clone())
        .map_err(|err| err.to_string())?;
    Ok(Task::perform(
        runtime::perform(async move {
            let result = inspect_host_governance_with_options(&host, options)
                .await
                .map_err(|err| err.to_string());
            CoreEvent::GovernanceLoaded {
                host_id,
                request_id,
                result,
            }
        }),
        Message::Core,
    ))
}

/// Builds and dispatches session creation from the Start modal. The input is the
/// prompt editor's text (typed for a blank session, or the edited template
/// prompt); branch/base come from the resolved template recipe when a template is
/// selected, otherwise from the Advanced overrides. The agent is the picker value.
fn create_session_task(app: &PohunekApp) -> Result<Task<Message>, String> {
    let target = project_target(app, app.start.project.as_ref())?;
    let host = target.host;
    let host_id = host.id.clone();
    let host_view = app
        .workspace
        .hosts
        .get(&host_id)
        .ok_or_else(|| format!("unknown host `{host_id}`"))?;
    ensure_agent_launchable(&host_id, host_view, &app.start.agent)?;
    let options = connection_options(app)?;
    let project = target.project_ref;
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
    let target = project_target(app, app.assistant.project.as_ref())?;
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
    let target = project_target(app, app.start.project.as_ref())?;
    let host = target.host;
    let options = connection_options(app)?;
    let project = target.project_ref;
    let generation = app.template_generation;
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
        move |result| Message::TemplateResolved { generation, result },
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

fn list_project_actions_task(
    app: &PohunekApp,
    project: &ProjectRef,
) -> Result<Task<Message>, String> {
    let target = project_target(app, Some(project))?;
    let host = target.host;
    let host_id = host.id.clone();
    let options = connection_options(app)?;
    let reference = target.project_ref;
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
        ConnState, HealthSummary, NotificationFilter, NotificationScope, PromptState,
        ProviderState, UiState, Workspace,
    };
    use protocol::{
        AgentKind, AgentRuntime, ApprovalKeyReference, HostGovernanceStatus,
        HostId as StableHostId, NotificationKind, NotificationRecord, NotificationSeverity,
        NotificationSource, ProjectInfo, ProjectSource, SessionInfo,
    };

    use super::*;
    use crate::message::MetadataEdit;

    fn project_ref(host: &str, project: &str) -> ProjectRef {
        ProjectRef {
            host_id: HostId::new(host),
            project_id: project.to_owned(),
        }
    }

    fn host_with(projects: &[(&str, &str)], agents: &[&str]) -> HostView {
        let mut host = test_host();
        host.projects = projects
            .iter()
            .map(|(id, label)| {
                let mut project = test_project();
                project.id = (*id).to_owned();
                project.label = (*label).to_owned();
                ((*id).to_owned(), project)
            })
            .collect();
        host.runtimes = agents
            .iter()
            .map(|agent| test_runtime(agent, None, true, None))
            .collect();
        host
    }

    /// Two connected hosts that both own a project labelled `api`; only `local`
    /// has a codex runtime, only `remote` has a claude runtime.
    fn app_with_two_hosts() -> PohunekApp {
        let mut app = app_without_selection();
        app.workspace.hosts.insert(
            HostId::new("local"),
            host_with(&[("p-1", "api"), ("p-2", "web")], &["codex"]),
        );
        app.workspace.hosts.insert(
            HostId::new("remote"),
            host_with(&[("p-9", "api")], &["claude"]),
        );
        app.hosts = vec![
            HostConfig::tcp("local", "127.0.0.1:9".parse().expect("inert address")),
            HostConfig::tcp("remote", "127.0.0.1:10".parse().expect("inert address")),
        ];
        app
    }

    fn test_session(id: &str, project: Option<&str>) -> SessionInfo {
        SessionInfo {
            name: None,
            id: SessionId(id.to_owned()),
            external: Some(false),
            capabilities: protocol::SessionCapabilities {
                resume: true,
                fork: true,
            },
            agent: "codex".to_owned(),
            agent_base: protocol::AgentKind::Codex,
            cwd: PathBuf::from("/work/project"),
            cwd_source: Some(protocol::CwdSource::Launch),
            pid: 42,
            cols: 80,
            rows: 24,
            state: protocol::SessionState::Running,
            state_source: protocol::StateSource::Process,
            activity: None,
            subagents: Vec::new(),
            active_agent: None,
            active_agent_base: None,
            active_agent_pid: None,
            active_agent_session_id: None,
            active_agent_session_path: None,
            native_session_id: Some("native-1".to_owned()),
            native_session_path: None,
            project_id: project.map(str::to_owned),
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

    #[test]
    fn changing_the_start_project_resets_template_actions_and_agent() {
        let mut app = app_with_two_hosts();
        app.workspace
            .hosts
            .get_mut(&HostId::new("local"))
            .expect("local host")
            .prompt
            .actions_by_project
            .insert(
                "p-1".to_owned(),
                protocol::ProjectActionsResult {
                    actions: vec![protocol::ActionSummary {
                        name: "review".to_owned(),
                        provider: protocol::ProviderKind::None,
                        template: "review".to_owned(),
                        layer: protocol::PromptLayer::Host,
                    }],
                },
            );
        app.start.project = Some(project_ref("local", "p-1"));
        app.start.agent = "codex".to_owned();
        app.start.template = Some("review".to_owned());
        app.template_recipe = Some(TemplateRecipe {
            agent: "codex".to_owned(),
            branch: None,
            base_branch: None,
        });
        app.prompt_editor = text_editor::Content::with_text("rendered template");
        assert_eq!(
            crate::selection::available_actions(&app, &protocol::ProviderKind::None),
            ["review"]
        );

        let _ = update(
            &mut app,
            Message::StartProjectSelected(project_ref("remote", "p-9")),
        );

        assert_eq!(app.start.project, Some(project_ref("remote", "p-9")));
        assert!(app.start.template.is_none());
        assert!(app.template_recipe.is_none());
        assert!(app.prompt_editor.text().trim().is_empty());
        assert_eq!(app.start.agent, "claude");
        assert!(
            crate::selection::available_actions(&app, &protocol::ProviderKind::None).is_empty()
        );
    }

    fn resolved(rendered: &str, agent: &str) -> ResolvedTemplate {
        ResolvedTemplate {
            rendered: rendered.to_owned(),
            recipe: TemplateRecipe {
                agent: agent.to_owned(),
                branch: Some("feature".to_owned()),
                base_branch: None,
            },
        }
    }

    fn template_reply(generation: u64, rendered: &str) -> Message {
        Message::TemplateResolved {
            generation,
            result: Ok(resolved(rendered, "claude")),
        }
    }

    #[test]
    fn template_reply_for_a_previous_project_is_ignored() {
        let mut app = app_with_two_hosts();
        app.start.project = Some(project_ref("local", "p-1"));
        app.start.template = Some("review".to_owned());
        let requested = app.template_generation;
        let _ = update(
            &mut app,
            Message::StartProjectSelected(project_ref("remote", "p-9")),
        );

        let _ = update(&mut app, template_reply(requested, "stale prompt"));

        assert!(app.template_recipe.is_none());
        assert!(app.prompt_editor.text().trim().is_empty());
        assert_eq!(app.start.agent, "claude");
    }

    #[test]
    fn template_reply_for_a_previous_template_is_ignored() {
        let mut app = app_with_two_hosts();
        app.start.project = Some(project_ref("local", "p-1"));
        let _ = update(
            &mut app,
            Message::StartTemplateSelected("review".to_owned()),
        );
        let requested = app.template_generation;
        let _ = update(
            &mut app,
            Message::StartTemplateSelected("deploy".to_owned()),
        );

        let _ = update(&mut app, template_reply(requested, "stale prompt"));

        assert!(app.template_recipe.is_none());
        assert!(app.prompt_editor.text().trim().is_empty());
    }

    #[test]
    fn matching_template_reply_applies_prompt_agent_and_recipe() {
        let mut app = app_with_two_hosts();
        app.start.project = Some(project_ref("local", "p-1"));
        let _ = update(
            &mut app,
            Message::StartTemplateSelected("review".to_owned()),
        );

        let current = app.template_generation;
        let _ = update(&mut app, template_reply(current, "fresh prompt"));

        assert_eq!(app.prompt_editor.text().trim(), "fresh prompt");
        assert_eq!(app.start.agent, "claude");
        assert_eq!(
            app.template_recipe
                .as_ref()
                .and_then(|r| r.branch.as_deref()),
            Some("feature")
        );
    }

    #[test]
    fn out_of_order_replies_for_the_same_template_keep_the_newest_and_later_edits() {
        let mut app = app_with_two_hosts();
        app.start.project = Some(project_ref("local", "p-1"));
        let _ = update(
            &mut app,
            Message::StartTemplateSelected("review".to_owned()),
        );
        let older = app.template_generation;
        let _ = update(
            &mut app,
            Message::StartTemplateSelected("review".to_owned()),
        );
        let newer = app.template_generation;
        assert_ne!(older, newer);

        let _ = update(&mut app, template_reply(newer, "newest prompt"));
        let _ = update(
            &mut app,
            Message::PromptEdited(text_editor::Action::Edit(text_editor::Edit::Insert('!'))),
        );
        let edited = app.prompt_editor.text();
        let _ = update(&mut app, template_reply(older, "older prompt"));

        assert_eq!(app.prompt_editor.text(), edited);
        assert!(edited.contains("newest prompt"));
        assert!(edited.contains('!'));
    }

    #[test]
    fn start_modal_with_many_projects_renders_every_option() {
        let mut app = app_with_two_hosts();
        let projects: Vec<(String, String)> = (0..40)
            .map(|index| (format!("p-{index:02}"), format!("project-{index:02}")))
            .collect();
        let refs: Vec<(&str, &str)> = projects
            .iter()
            .map(|(id, label)| (id.as_str(), label.as_str()))
            .collect();
        app.workspace
            .hosts
            .insert(HostId::new("local"), host_with(&refs, &["codex"]));
        app.modal = ModalView::Start;
        app.form_select = Some(FormSelect {
            field: FormField::StartProject,
            cursor: 0,
        });

        let local_options = keyboard::form_select_options(&app, FormField::StartProject)
            .into_iter()
            .filter(|option| option.ends_with("local"))
            .count();
        assert_eq!(local_options, 40);
        let _ = crate::view::view(&app);
    }

    #[test]
    fn same_labelled_projects_on_one_host_get_distinct_picker_labels() {
        let mut app = app_with_two_hosts();
        app.workspace.hosts.insert(
            HostId::new("local"),
            host_with(&[("p-1", "api"), ("p-2", "api")], &["codex"]),
        );
        app.modal = ModalView::Start;

        let options = keyboard::form_select_options(&app, FormField::StartProject);
        assert_eq!(
            options,
            [
                "api  ·  local  ·  p-1",
                "api  ·  local  ·  p-2",
                "api  ·  remote"
            ]
        );
        let pick = |cursor| {
            keyboard::form_select_choice_message(
                &app,
                FormSelect {
                    field: FormField::StartProject,
                    cursor,
                },
            )
        };
        assert!(matches!(
            pick(1),
            Some(Message::StartProjectSelected(project)) if project == project_ref("local", "p-2")
        ));
        app.start.project = Some(project_ref("local", "p-2"));
        assert_eq!(
            keyboard::form_select_label(&app, FormField::StartProject),
            "api  ·  local  ·  p-2"
        );
    }

    #[test]
    fn reselecting_the_same_start_project_keeps_the_template() {
        let mut app = app_with_two_hosts();
        app.start.project = Some(project_ref("local", "p-1"));
        app.start.template = Some("review".to_owned());

        let _ = update(
            &mut app,
            Message::StartProjectSelected(project_ref("local", "p-1")),
        );

        assert_eq!(app.start.template.as_deref(), Some("review"));
    }

    #[test]
    fn assistant_project_change_drops_an_agent_the_new_host_cannot_run() {
        let mut app = app_with_two_hosts();
        app.assistant.project = Some(project_ref("local", "p-1"));
        app.assistant.agent = Some("codex".to_owned());

        let _ = update(
            &mut app,
            Message::AssistantProjectSelected(project_ref("remote", "p-9")),
        );

        assert_eq!(app.assistant.project, Some(project_ref("remote", "p-9")));
        assert!(app.assistant.agent.is_none());
    }

    #[test]
    fn start_modal_preselects_filter_then_selected_session_then_only_project() {
        let mut app = app_with_two_hosts();
        let local = HostId::new("local");
        app.workspace
            .hosts
            .get_mut(&local)
            .expect("local host")
            .sessions
            .insert("s-1".to_owned(), test_session("s-1", Some("p-2")));

        let _ = update(&mut app, Message::OpenStartModal);
        assert_eq!(app.modal, ModalView::Start);
        assert_eq!(app.form_focus, FormField::StartProject);
        assert!(app.start.project.is_none(), "several projects, no hint");

        app.ui_state.selection = Some(Selection::Session {
            host_id: local.clone(),
            session_id: SessionId("s-1".to_owned()),
        });
        let _ = update(&mut app, Message::OpenStartModal);
        assert_eq!(app.start.project, Some(project_ref("local", "p-2")));

        app.project_filter = Some(project_ref("remote", "p-9"));
        let _ = update(&mut app, Message::OpenStartModal);
        assert_eq!(app.start.project, Some(project_ref("remote", "p-9")));

        let mut single = app_without_selection();
        single
            .workspace
            .hosts
            .insert(local, host_with(&[("p-1", "api")], &["codex"]));
        let _ = update(&mut single, Message::OpenStartModal);
        assert_eq!(single.start.project, Some(project_ref("local", "p-1")));
    }

    #[test]
    fn assistant_modal_preselects_like_the_start_modal() {
        let mut app = app_with_two_hosts();
        app.project_filter = Some(project_ref("local", "p-2"));

        let _ = update(&mut app, Message::OpenAssistantModal);

        assert_eq!(app.assistant.project, Some(project_ref("local", "p-2")));
        assert_eq!(app.form_focus, FormField::AssistantProject);
    }

    #[test]
    fn creating_a_session_targets_the_form_projects_host() {
        let mut app = app_with_two_hosts();
        app.ui_state.selection = Some(Selection::Session {
            host_id: HostId::new("local"),
            session_id: SessionId("s-1".to_owned()),
        });
        app.start.project = Some(project_ref("remote", "p-9"));
        app.start.agent = "codex".to_owned();

        let target = crate::selection::project_target(&app, app.start.project.as_ref())
            .expect("form project target");
        assert_eq!(target.host.id, HostId::new("remote"));
        assert_eq!(target.project_ref, "p-9");

        let Err(err) = create_session_task(&app) else {
            panic!("codex is not launchable on the form project's host");
        };
        assert!(err.contains("`remote`"), "{err}");

        app.start.project = None;
        let Err(err) = create_session_task(&app) else {
            panic!("a project is required");
        };
        assert_eq!(err, "choose a project first");
    }

    #[test]
    fn project_filter_clears_when_its_project_has_no_sessions_left() {
        let mut app = app_with_governance_host();
        let route = HostId::new("local");
        let mut with_session = governance_snapshot("local");
        with_session.projects = vec![test_project()];
        with_session.sessions = vec![test_session("s-1", Some("p-1"))];
        let _ = update(
            &mut app,
            Message::Core(CoreEvent::HostSnapshotLoaded {
                snapshot: with_session,
            }),
        );
        let _ = update(
            &mut app,
            Message::SetProjectFilter(Some(project_ref("local", "p-1"))),
        );

        let _ = update(
            &mut app,
            Message::Core(CoreEvent::HostOperationFailed {
                host_id: route.clone(),
                error: "unrelated".to_owned(),
            }),
        );
        assert_eq!(app.project_filter, Some(project_ref("local", "p-1")));

        let mut emptied = governance_snapshot("local");
        emptied.projects = vec![test_project()];
        let _ = update(
            &mut app,
            Message::Core(CoreEvent::HostSnapshotLoaded { snapshot: emptied }),
        );
        assert!(app.project_filter.is_none());
    }

    fn app_with_sessions_in_two_projects() -> PohunekApp {
        let mut app = app_with_governance_host();
        let mut snapshot = governance_snapshot("local");
        let mut second = test_project();
        second.id = "p-2".to_owned();
        second.label = "Other".to_owned();
        snapshot.projects = vec![test_project(), second];
        snapshot.sessions = vec![
            test_session("s-1", Some("p-1")),
            test_session("s-2", Some("p-2")),
        ];
        let _ = update(
            &mut app,
            Message::Core(CoreEvent::HostSnapshotLoaded { snapshot }),
        );
        app
    }

    fn global_key_messages(app: &PohunekApp, key: &iced::keyboard::Key) -> usize {
        keyboard::route_key_press(app, key, iced::keyboard::Modifiers::empty()).len()
    }

    #[test]
    fn removing_the_form_project_clears_it_and_keeps_the_typed_text() {
        let mut app = app_with_sessions_in_two_projects();
        let _ = update(&mut app, Message::OpenStartModal);
        let _ = update(
            &mut app,
            Message::StartProjectSelected(project_ref("local", "p-2")),
        );
        app.start.name = "my name".to_owned();
        app.start.template = Some("review".to_owned());
        app.prompt_editor = text_editor::Content::with_text("typed prompt");
        let generation = app.template_generation;

        let mut without_p2 = governance_snapshot("local");
        without_p2.projects = vec![test_project()];
        without_p2.sessions = vec![test_session("s-1", Some("p-1"))];
        let _ = update(
            &mut app,
            Message::Core(CoreEvent::HostSnapshotLoaded {
                snapshot: without_p2,
            }),
        );

        assert_eq!(app.start.project, Some(project_ref("local", "p-1")));
        assert!(app.start.template.is_none() && app.template_recipe.is_none());
        assert!(app.template_generation > generation);
        assert_eq!(app.start.name, "my name");
        assert_eq!(app.prompt_editor.text(), "typed prompt");
        assert_eq!(app.modal, ModalView::Start);
    }

    #[test]
    fn removing_the_only_form_project_leaves_the_form_unset_and_unsendable() {
        let mut app = app_with_sessions_in_two_projects();
        app.start.project = Some(project_ref("local", "p-1"));
        app.assistant.project = Some(project_ref("local", "p-1"));
        app.prompt_editor = text_editor::Content::with_text("typed prompt");

        let _ = update(
            &mut app,
            Message::Core(CoreEvent::HostSnapshotLoaded {
                snapshot: governance_snapshot("local"),
            }),
        );

        assert!(app.start.project.is_none());
        assert!(app.assistant.project.is_none());
        assert_eq!(app.prompt_editor.text(), "typed prompt");
        let Err(err) = create_session_task(&app) else {
            panic!("a form without a project must not send");
        };
        assert_eq!(err, "choose a project first");
    }

    #[test]
    fn a_stale_form_project_is_rejected_before_sending() {
        let mut app = app_with_two_hosts();
        app.start.project = Some(project_ref("local", "gone"));
        app.start.agent = "codex".to_owned();

        assert!(crate::selection::project_host(&app, app.start.project.as_ref()).is_none());
        let Err(err) = create_session_task(&app) else {
            panic!("a project the host no longer lists must not send");
        };
        assert!(err.contains("no longer available"), "{err}");
    }

    #[test]
    fn changing_the_filter_drops_a_selection_it_hides() {
        use iced::keyboard::key::Named;
        use iced::keyboard::Key;

        let mut app = app_with_sessions_in_two_projects();
        let _ = update(
            &mut app,
            Message::SelectSession {
                host_id: HostId::new("local"),
                session_id: SessionId("s-2".to_owned()),
            },
        );
        let _ = update(&mut app, Message::CloseModal);
        assert!(global_key_messages(&app, &Key::Character("o".into())) > 0);
        assert!(global_key_messages(&app, &Key::Named(Named::Enter)) > 0);

        let _ = update(
            &mut app,
            Message::SetProjectFilter(Some(project_ref("local", "p-1"))),
        );

        assert!(app.ui_state.selection.is_none());
        assert!(app.workspace.selection.is_none());
        assert_eq!(global_key_messages(&app, &Key::Character("o".into())), 0);
        assert_eq!(global_key_messages(&app, &Key::Named(Named::Enter)), 0);
    }

    #[test]
    fn changing_the_filter_keeps_a_selection_it_still_shows() {
        let mut app = app_with_sessions_in_two_projects();
        let _ = update(
            &mut app,
            Message::SelectSession {
                host_id: HostId::new("local"),
                session_id: SessionId("s-1".to_owned()),
            },
        );
        let _ = update(&mut app, Message::CloseModal);

        let _ = update(
            &mut app,
            Message::SetProjectFilter(Some(project_ref("local", "p-1"))),
        );

        assert_eq!(
            app.ui_state.selection,
            Some(Selection::Session {
                host_id: HostId::new("local"),
                session_id: SessionId("s-1".to_owned()),
            })
        );
        assert!(
            global_key_messages(
                &app,
                &iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter)
            ) > 0
        );
    }

    #[test]
    fn same_labelled_projects_on_different_hosts_stay_distinct() {
        let mut app = app_with_two_hosts();
        app.modal = ModalView::Start;

        let options = keyboard::form_select_options(&app, FormField::StartProject);
        assert_eq!(
            options,
            ["api  ·  local", "api  ·  remote", "web  ·  local"]
        );

        let pick = |cursor| {
            keyboard::form_select_choice_message(
                &app,
                FormSelect {
                    field: FormField::StartProject,
                    cursor,
                },
            )
        };
        assert!(matches!(
            pick(0),
            Some(Message::StartProjectSelected(project)) if project == project_ref("local", "p-1")
        ));
        assert!(matches!(
            pick(1),
            Some(Message::StartProjectSelected(project)) if project == project_ref("remote", "p-9")
        ));

        app.start.project = Some(project_ref("remote", "p-9"));
        assert_eq!(
            keyboard::form_select_label(&app, FormField::StartProject),
            "api  ·  remote"
        );
        assert_eq!(
            keyboard::form_select_cursor(&app, FormField::StartProject),
            1
        );
    }

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
            form_focus: crate::message::FormField::StartAgent,
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
            notification_health: crate::notify::NotificationHealth::default(),
        }
    }

    #[test]
    fn assistant_select_cursor_wraps_in_both_directions() {
        let mut app = app_without_selection();
        app.modal = ModalView::Assistant;
        app.form_focus = FormField::AssistantIntent;
        app.form_select = Some(FormSelect {
            field: FormField::AssistantIntent,
            cursor: 0,
        });

        move_form_select(&mut app, ListDirection::Up);
        assert_eq!(app.form_select.expect("open select").cursor, 4);

        move_form_select(&mut app, ListDirection::Down);
        assert_eq!(app.form_select.expect("open select").cursor, 0);
    }

    #[test]
    fn empty_start_agent_select_stays_closed() {
        let mut app = app_without_selection();
        app.modal = ModalView::Start;

        let _ = update(&mut app, Message::ToggleFormSelect(FormField::StartAgent));

        assert_eq!(app.form_focus, FormField::StartAgent);
        assert!(app.form_select.is_none());
    }

    #[test]
    fn governance_snapshot_dispatches_once_and_completion_does_not_loop() {
        let mut app = app_with_governance_host();
        let route = HostId::new("local");
        let _snapshot_task = update(
            &mut app,
            Message::Core(CoreEvent::HostSnapshotLoaded {
                snapshot: governance_snapshot("local"),
            }),
        );
        let request_id = match app.workspace.governance(&route) {
            Some(pohunek_gui_core::GovernanceState::Loading { request_id }) => *request_id,
            other => panic!("snapshot must dispatch one governance request, got {other:?}"),
        };
        assert_eq!(request_id.get(), 1);

        let _completion_task = update(
            &mut app,
            Message::Core(CoreEvent::GovernanceLoaded {
                host_id: route.clone(),
                request_id,
                result: Ok(never_enrolled_governance_status()),
            }),
        );
        assert!(matches!(
            app.workspace.governance(&route),
            Some(pohunek_gui_core::GovernanceState::Loaded(status)) if status.enrollment().is_none()
        ));
        let next = app
            .workspace
            .begin_governance_request(route)
            .expect("completion must not have dispatched another request");
        assert_eq!(next.get(), 2);
    }

    #[test]
    fn governance_error_completion_does_not_dispatch_follow_up_request() {
        let mut app = app_with_governance_host();
        let route = HostId::new("local");
        let _snapshot_task = update(
            &mut app,
            Message::Core(CoreEvent::HostSnapshotLoaded {
                snapshot: governance_snapshot("local"),
            }),
        );
        let request_id = match app.workspace.governance(&route) {
            Some(pohunek_gui_core::GovernanceState::Loading { request_id }) => *request_id,
            other => panic!("snapshot must dispatch one governance request, got {other:?}"),
        };
        assert_eq!(request_id.get(), 1);

        let _completion_task = update(
            &mut app,
            Message::Core(CoreEvent::GovernanceLoaded {
                host_id: route.clone(),
                request_id,
                result: Err("unreachable".to_owned()),
            }),
        );
        assert_eq!(
            app.workspace.governance(&route),
            Some(&pohunek_gui_core::GovernanceState::Error(
                "unreachable".to_owned()
            ))
        );

        let next = app
            .workspace
            .begin_governance_request(route)
            .expect("error completion must not have dispatched another request");
        assert_eq!(next.get(), 2);
    }

    fn app_with_governance_host() -> PohunekApp {
        let mut app = app_without_selection();
        let local_host = HostConfig::tcp(
            "local",
            "127.0.0.1:9".parse().expect("valid inert test address"),
        );
        app.hosts = vec![local_host.clone()];
        app.config = Ok(AppConfig {
            attach: crate::config::AttachSelection::Command {
                template: "attach {host} {id}".to_owned(),
                mode: crate::config::AttachCommandMode::Shell,
            },
            pohunek_bin: "pohunek".to_owned(),
            launch: crate::config::LaunchSettings {
                open_timeout: std::time::Duration::from_secs(1),
                login_shell_timeout: std::time::Duration::from_secs(1),
                login_shell_max_output_bytes: 1024,
                notification_timeout: std::time::Duration::from_secs(1),
                attach_observe: std::time::Duration::from_millis(50),
                attach_script_max_age: std::time::Duration::from_secs(3600),
            },
            bin_resolver: std::sync::Arc::new(crate::bin_resolver::BinResolver::with_discovery(
                "pohunek",
                || Err(crate::bin_resolver::BinError::SearchPath("test".to_owned())),
            )),
            local_host,
            connection_options: ConnectionOptions::default(),
            terminal_size: crate::config::TerminalSize::default(),
            notification: crate::notify::Notifier {
                backend: crate::notify::NotificationBackend::Osascript {
                    executable: "/nonexistent/osascript".into(),
                },
                timeout: std::time::Duration::from_secs(1),
            },
            keymap: keyboard::KeyMap::default(),
        });
        app
    }

    fn governance_snapshot(host_id: &str) -> pohunek_gui_core::HostSnapshot {
        pohunek_gui_core::HostSnapshot {
            host_id: HostId::new(host_id),
            health: HealthSummary {
                status: "ok".to_owned(),
                daemon_version: "test".to_owned(),
                protocol_version: protocol::PROTOCOL_VERSION,
            },
            sessions: Vec::new(),
            projects: Vec::new(),
            project_error: None,
            notifications: Vec::new(),
            supported_agents: Vec::new(),
            runtimes: Vec::new(),
            notification_providers: Vec::new(),
            observation_capabilities: pohunek_gui_core::ObservationCapabilities::default(),
        }
    }

    fn never_enrolled_governance_status() -> HostGovernanceStatus {
        HostGovernanceStatus::new(
            StableHostId::parse("host_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
                .expect("valid stable host id"),
            None,
            None,
            None,
            None,
            ApprovalKeyReference::from_ed25519_verifying_key_bytes([7; 32]),
        )
        .expect("valid never-enrolled governance status")
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
            governance: pohunek_gui_core::GovernanceState::default(),
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
            repo_root: PathBuf::from("/work/project"),
            git_common_dir: PathBuf::from("/work/project/.git"),
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
