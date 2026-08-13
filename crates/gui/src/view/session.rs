//! Session detail modal, lifecycle actions, and metadata editor.

use iced::widget::{button, column, row, scrollable, text, text_input};
use iced::{Center, Element, Fill};
use pohunek_gui_core::{session_metadata_rows, HostId, RuntimeContinuity, SessionAccess};
use protocol::{AgentActivity, CwdSource, NotificationStatus, SessionInfo};

use crate::message::Message;
use crate::selection::selected_session;
use crate::PohunekApp;

use super::{
    card, dialog_card, muted_style, section_title, selectable_text, session_agent_label,
    status_pill, PillTone,
};

/// Session-detail dialog opened from the prioritized list.
pub(crate) fn session_modal_content(app: &PohunekApp) -> Element<'_, Message> {
    dialog_card(
        "Session",
        scrollable(session_pane(app)).height(Fill).width(Fill),
    )
}

/// Confirms permanent deletion of the selected logical session.
pub(crate) fn confirm_delete_modal_content(app: &PohunekApp) -> Element<'_, Message> {
    let description = selected_session(app).map_or_else(
        || "The selected session is no longer available.".to_owned(),
        |(host_id, session)| {
            let name = session.name.as_deref().unwrap_or(session.id.0.as_str());
            format!(
                "Delete {name} ({host_id} / {})? This removes its logical record, retained session logs, and any eligible Pohunek-owned worktree.",
                session.id.0
            )
        },
    );
    dialog_card(
        "Delete session",
        column![
            text(description).size(14),
            row![
                button("Cancel")
                    .on_press(Message::CloseModal)
                    .style(iced::widget::button::secondary),
                button("Delete session")
                    .on_press(Message::ConfirmDeleteSession)
                    .style(iced::widget::button::danger),
            ]
            .spacing(8),
        ]
        .spacing(14),
    )
}

/// Session surface: the session card with its actions and metadata.
pub(crate) fn session_pane(app: &PohunekApp) -> Element<'_, Message> {
    session_detail(app)
}

fn session_detail(app: &PohunekApp) -> Element<'_, Message> {
    let mut detail = column![section_title("Session")].spacing(8);
    match selected_session(app) {
        Some((host_id, session)) => {
            let external = is_external_session(session);
            let activity = session.activity.map_or("unknown", session_activity_label);
            // Lead with the display name when set, keeping host/id as a subtitle
            // so the session stays identifiable.
            let heading = match &session.name {
                Some(name) => format!("{name}  ·  {host_id} / {}", session.id.0),
                None => format!("{} / {}", host_id, session.id.0),
            };
            detail = detail
                .push(selectable_text(heading).size(16))
                .push(selectable_text(format!("agent: {}", session_agent_label(session))).size(14))
                .push(selectable_text(origin_label(session)).size(14))
                .push(selectable_text(format!("state: {}", session.state.as_str())).size(14))
                .push(selectable_text(format!("activity: {activity}")).size(14));
            detail = detail.push(session_attention_view(app, host_id, session));
            detail = session_runtime_details(detail, app, host_id, session);
            if let Some(project) = session
                .project_label
                .as_ref()
                .or(session.project_id.as_ref())
            {
                detail = detail.push(selectable_text(format!("project: {project}")).size(14));
            }
            if let Some(branch) = &session.branch {
                detail = detail.push(selectable_text(format!("branch: {branch}")).size(14));
            }
            if let Some(path) = &session.worktree_path {
                detail =
                    detail.push(selectable_text(format!("worktree: {}", path.display())).size(14));
            }
            detail =
                detail.push(selectable_text(format!("cwd: {}", session.cwd.display())).size(14));
            detail = detail.push(
                selectable_text(format!(
                    "cwd source: {}",
                    cwd_source_label(session.cwd_source)
                ))
                .size(14),
            );
            if has_worktree_drift(session) {
                detail = detail.push(selectable_text("worktree drift: yes").size(14));
            }
            detail = detail.push(session_actions(app, host_id, session, external));
            if let Some(observation) = session_observation_view(app, host_id, session) {
                detail = detail.push(observation);
            }
            detail = detail.push(session_activity_view(app, host_id, session));
            if !external {
                detail = detail.push(rename_view(app));
                detail = detail.push(metadata_view(app, session));
            }
        }
        None => {
            detail = detail.push(selectable_text("No session selected").size(16));
        }
    }
    card(detail)
}

fn session_attention_view(
    app: &PohunekApp,
    host_id: &HostId,
    session: &SessionInfo,
) -> Element<'static, Message> {
    let attention = app
        .workspace
        .session_rows()
        .into_iter()
        .find(|row| row.host_id == *host_id && row.session_id == session.id)
        .and_then(|row| row.attention);
    let content = if let Some(attention) = attention {
        column![
            row![
                text("Current attention").size(14),
                status_pill("Needs you", PillTone::Danger),
            ]
            .spacing(6)
            .align_y(Center),
            text(attention.title).size(13),
        ]
        .spacing(4)
    } else {
        column![
            text("Current attention").size(14),
            text("Nothing is waiting for you.")
                .size(12)
                .style(muted_style),
        ]
        .spacing(4)
    };
    card(content)
}

fn session_activity_view(
    app: &PohunekApp,
    host_id: &HostId,
    session: &SessionInfo,
) -> Element<'static, Message> {
    let records = app.workspace.session_activity(host_id, &session.id);
    let mut activity = column![row![
        text("Recent activity").size(14),
        iced::widget::space().width(Fill),
        button("Open Activity")
            .on_press(Message::OpenHostInbox(host_id.clone()))
            .style(iced::widget::button::text),
    ]
    .align_y(Center),]
    .spacing(4);
    if records.is_empty() {
        activity = activity.push(text("No recorded activity.").size(12).style(muted_style));
    } else {
        for record in records.into_iter().take(5) {
            let read_state = match record.status {
                NotificationStatus::Unread => "unread",
                NotificationStatus::Read => "read",
                NotificationStatus::Acknowledged => "resolved",
                NotificationStatus::Archived => "archived",
                NotificationStatus::Deleted => "deleted",
            };
            activity = activity.push(
                text(format!(
                    "{}  ·  {}  ·  {}",
                    record.title, read_state, record.created_at
                ))
                .size(12),
            );
        }
    }
    card(activity)
}

fn session_activity_label(activity: AgentActivity) -> &'static str {
    match activity {
        AgentActivity::Idle => "ready",
        AgentActivity::Working => "working",
        AgentActivity::Blocked => "waiting for input",
    }
}

fn session_runtime_details<'a>(
    mut detail: iced::widget::Column<'a, Message>,
    app: &PohunekApp,
    host_id: &HostId,
    session: &SessionInfo,
) -> iced::widget::Column<'a, Message> {
    let Some(runtime) = &session.runtime else {
        return detail;
    };

    detail = detail.push(selectable_text(format!("runtime: {}", runtime.state.as_str())).size(14));
    if let Some(continuity) = app.workspace.runtime_continuity(host_id, &session.id) {
        detail = detail.push(selectable_text(runtime_continuity_label(continuity)).size(14));
    }
    if let Some(reason) = &runtime.loss_reason {
        detail = detail.push(selectable_text(format!("runtime reason: {reason}")).size(14));
    }
    if let Some(runtime_id) = &runtime.runtime_id {
        detail = detail.push(selectable_text(format!("runtime id: {runtime_id}")).size(14));
    }
    if let Some(worker_id) = &runtime.worker_id {
        detail = detail.push(selectable_text(format!("worker id: {worker_id}")).size(14));
    }

    detail
}

fn session_observation_view(
    app: &PohunekApp,
    host_id: &HostId,
    session: &SessionInfo,
) -> Option<Element<'static, Message>> {
    let observation = app.workspace.session_observation(host_id, &session.id)?;
    let mut content = column![].spacing(4);
    if let Some(screen) = &observation.screen {
        content = content.push(text("Terminal screen").size(14)).push(
            selectable_text(screen.visible_lines.join("\n"))
                .size(12)
                .font(iced::Font::MONOSPACE),
        );
    }
    if let Some((start, end)) = observation.output_gap {
        content = content.push(
            selectable_text(format!(
                "output gap: {start}..{end}; showing retained bytes"
            ))
            .size(12),
        );
    }
    if !observation.output_text.is_empty() {
        content = content.push(text("Retained output").size(14)).push(
            selectable_text(observation.output_text.clone())
                .size(12)
                .font(iced::Font::MONOSPACE),
        );
    }
    if let Some(wait) = &observation.wait {
        content = content.push(
            selectable_text(format!("last wait: {}", wait_reason_label(wait.reason))).size(12),
        );
    }
    Some(content.into())
}

fn session_actions<'a>(
    app: &'a PohunekApp,
    host_id: &'a HostId,
    session: &'a SessionInfo,
    external: bool,
) -> Element<'a, Message> {
    let mut actions = row![button("Inspect")
        .on_press(Message::InspectSelectedSession)
        .style(iced::widget::button::secondary)]
    .spacing(8);
    if !external {
        let observation = app
            .workspace
            .hosts
            .get(host_id)
            .map(|host| host.observation_capabilities)
            .unwrap_or_default();
        let row = app
            .workspace
            .session_rows()
            .into_iter()
            .find(|row| row.host_id == *host_id && row.session_id == session.id);
        let access = row
            .as_ref()
            .map_or(SessionAccess::Unavailable, |row| row.access);
        let can_open = matches!(access, SessionAccess::Attach | SessionAccess::Resume);
        let open_label = if access == SessionAccess::Resume {
            "Resume in terminal"
        } else if can_open {
            "Open in terminal"
        } else {
            "Open unavailable"
        };
        let mut open_button = button(open_label).style(iced::widget::button::primary);
        if can_open {
            open_button = open_button.on_press(Message::OpenSession {
                host_id: host_id.clone(),
                session_id: session.id.clone(),
            });
        }
        let mut fork_button = button(if session.capabilities.fork {
            "Fork"
        } else {
            "Fork unsupported"
        })
        .style(iced::widget::button::secondary);
        if session.capabilities.fork {
            fork_button = fork_button.on_press(Message::ForkSelectedSession);
        }
        actions = actions
            .push(open_button)
            .push(fork_button)
            .push(optional_action_button(
                "Read screen",
                observation.terminal_read,
                Message::ReadSelectedSessionScreen,
            ))
            .push(optional_action_button(
                "Read output",
                observation.output_read,
                Message::ReadSelectedSessionOutput,
            ))
            .push(optional_action_button(
                "Wait for change",
                observation.session_wait,
                Message::WaitForSelectedSession,
            ));
        if row.as_ref().is_some_and(|row| row.can_stop) {
            actions = actions.push(
                button("Terminate")
                    .on_press(Message::StopSession {
                        host_id: host_id.clone(),
                        session_id: session.id.clone(),
                    })
                    .style(iced::widget::button::danger),
            );
        }
        if row.as_ref().is_some_and(|row| row.can_remove) {
            actions = actions.push(
                button("Delete")
                    .on_press(Message::RequestDeleteSession {
                        host_id: host_id.clone(),
                        session_id: session.id.clone(),
                    })
                    .style(iced::widget::button::danger),
            );
        }
    }
    actions.into()
}

fn optional_action_button(
    label: &str,
    supported: bool,
    message: Message,
) -> iced::widget::Button<'_, Message> {
    let mut action = button(if supported { label } else { "Unsupported" })
        .style(iced::widget::button::secondary);
    if supported {
        action = action.on_press(message);
    }
    action
}

fn wait_reason_label(reason: protocol::SessionWaitReason) -> &'static str {
    match reason {
        protocol::SessionWaitReason::StateMatched => "state matched",
        protocol::SessionWaitReason::ActivityMatched => "activity matched",
        protocol::SessionWaitReason::SessionUpdated => "session updated",
        protocol::SessionWaitReason::TerminalChanged => "terminal changed",
        protocol::SessionWaitReason::OutputAdvanced => "output advanced",
        protocol::SessionWaitReason::RuntimeChanged => "runtime changed",
        protocol::SessionWaitReason::Timeout => "timeout",
    }
}

fn cwd_source_label(source: Option<CwdSource>) -> &'static str {
    source.map_or("unknown", CwdSource::as_str)
}

fn origin_label(session: &SessionInfo) -> &'static str {
    if is_external_session(session) {
        "origin: external (read-only)"
    } else {
        "origin: managed"
    }
}

fn runtime_continuity_label(continuity: RuntimeContinuity) -> &'static str {
    match continuity {
        RuntimeContinuity::Reconnected => "runtime continuity: reconnected to the same PTY",
        RuntimeContinuity::Recovered => "runtime continuity: recovered into a new PTY generation",
    }
}

fn is_external_session(session: &SessionInfo) -> bool {
    session.external == Some(true)
}

fn has_worktree_drift(session: &SessionInfo) -> bool {
    session
        .worktree_path
        .as_ref()
        .is_some_and(|worktree_path| !session.cwd.starts_with(worktree_path))
}

/// Rename control for the selected session: a name field plus set/clear buttons,
/// wired to the shared rename buffer. Clearing reverts the session to id-only
/// display.
fn rename_view(app: &PohunekApp) -> Element<'_, Message> {
    column![
        text("Rename").size(16),
        row![
            text_input("new session name", &app.rename_edit)
                .on_input(Message::RenameEditChanged)
                .on_submit(Message::RenameSession),
            button("Rename")
                .on_press(Message::RenameSession)
                .style(iced::widget::button::secondary),
            button("Clear name")
                .on_press(Message::ClearSessionName)
                .style(iced::widget::button::secondary),
        ]
        .spacing(8),
    ]
    .spacing(6)
    .into()
}

fn metadata_view<'a>(app: &'a PohunekApp, session: &'a SessionInfo) -> Element<'a, Message> {
    let mut metadata = column![text("Metadata").size(16)].spacing(6);
    let rows = session_metadata_rows(session);
    if rows.is_empty() {
        metadata = metadata.push(text("No metadata").size(13));
    } else {
        for row in rows {
            metadata =
                metadata.push(selectable_text(format!("{} = {}", row.key, row.value)).size(13));
        }
    }
    metadata = metadata
        .push(
            row![
                text_input("key", &app.metadata_edit.key).on_input(Message::MetadataKeyChanged),
                text_input("value", &app.metadata_edit.value)
                    .on_input(Message::MetadataValueChanged)
            ]
            .spacing(8),
        )
        .push(
            row![
                button("Set metadata")
                    .on_press(Message::SetMetadata)
                    .style(iced::widget::button::secondary),
                button("Clear key")
                    .on_press(Message::ClearMetadata)
                    .style(iced::widget::button::secondary)
            ]
            .spacing(8),
        );
    metadata.into()
}

/// A labeled "Name" text input bound to the shared start-form name buffer, used
/// by every session-creation surface so a session can be named at any creation.
pub(crate) fn session_name_input(app: &PohunekApp) -> Element<'_, Message> {
    row![
        text("Name").size(14),
        text_input("optional session name", &app.start.name)
            .id(crate::keyboard::start_name_input_id())
            .on_input(Message::StartNameChanged),
    ]
    .spacing(8)
    .align_y(Center)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_neutral_wait_reasons_have_operator_labels() {
        assert_eq!(
            wait_reason_label(protocol::SessionWaitReason::RuntimeChanged),
            "runtime changed"
        );
        assert_eq!(
            wait_reason_label(protocol::SessionWaitReason::OutputAdvanced),
            "output advanced"
        );
    }

    #[test]
    fn observation_action_buttons_render_for_supported_and_unsupported_hosts() {
        let _: iced::widget::Button<'_, Message> =
            optional_action_button("Read screen", true, Message::ReadSelectedSessionScreen);
        let _: iced::widget::Button<'_, Message> =
            optional_action_button("Read screen", false, Message::ReadSelectedSessionScreen);
    }
}
