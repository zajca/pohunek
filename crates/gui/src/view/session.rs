//! Session detail pane: summary, rename control, and metadata editor.

use iced::widget::{button, column, row, text, text_input};
use iced::{Center, Element};
use pohunek_gui_core::{
    session_link_metadata, session_metadata_rows, HostId, RuntimeContinuity, SessionLinkKind,
    SessionLinkProvider,
};
use protocol::{CwdSource, SessionInfo};

use crate::attach::{session_can_open, session_requires_resume_before_attach};
use crate::message::Message;
use crate::selection::{selected_host_config, selected_session};
use crate::view::provider::linked_github_status;
use crate::PohunekApp;

use super::{card, section_title, session_agent_label};

/// Session surface: the session card with its actions and metadata.
pub(crate) fn session_pane(app: &PohunekApp) -> Element<'_, Message> {
    session_detail(app)
}

fn session_detail(app: &PohunekApp) -> Element<'_, Message> {
    let mut detail = column![section_title("Session")].spacing(8);
    match selected_session(app) {
        Some((host_id, session)) => {
            let external = is_external_session(session);
            let activity = session
                .activity
                .map_or("unknown", |activity| activity.as_str());
            // Lead with the display name when set, keeping host/id as a subtitle
            // so the session stays identifiable.
            let heading = match &session.name {
                Some(name) => format!("{name}  ·  {host_id} / {}", session.id.0),
                None => format!("{} / {}", host_id, session.id.0),
            };
            detail = detail
                .push(text(heading).size(16))
                .push(text(format!("agent: {}", session_agent_label(session))).size(14))
                .push(text(origin_label(session)).size(14))
                .push(text(format!("state: {}", session.state.as_str())).size(14))
                .push(text(format!("activity: {activity}")).size(14));
            if let Some(runtime) = &session.runtime {
                detail = detail.push(text(format!("runtime: {}", runtime.state.as_str())).size(14));
                if let Some(continuity) = app.workspace.runtime_continuity(host_id, &session.id) {
                    detail = detail.push(text(runtime_continuity_label(continuity)).size(14));
                }
                if let Some(reason) = &runtime.loss_reason {
                    detail = detail.push(text(format!("runtime reason: {reason}")).size(14));
                }
                if let Some(runtime_id) = &runtime.runtime_id {
                    detail = detail.push(text(format!("runtime id: {runtime_id}")).size(14));
                }
                if let Some(worker_id) = &runtime.worker_id {
                    detail = detail.push(text(format!("worker id: {worker_id}")).size(14));
                }
            }
            if let Some(project) = session
                .project_label
                .as_ref()
                .or(session.project_id.as_ref())
            {
                detail = detail.push(text(format!("project: {project}")).size(14));
            }
            if let Some(branch) = &session.branch {
                detail = detail.push(text(format!("branch: {branch}")).size(14));
            }
            if let Some(link) = session_link_metadata(session) {
                detail = detail.push(text(format!(
                    "linked: {} {} {}",
                    link.provider.as_str(),
                    link.kind.as_str(),
                    link.id
                )));
                if link.provider == SessionLinkProvider::GitHub
                    && link.kind == SessionLinkKind::PullRequest
                {
                    let status = selected_host_config(app)
                        .ok()
                        .and_then(|host| app.workspace.hosts.get(&host.id))
                        .and_then(|host| linked_github_status(host, session));
                    detail = detail.push(text(format!(
                        "PR status: {}",
                        status.unwrap_or_else(|| "unknown".to_owned())
                    )));
                    detail = detail.push(
                        button("Refresh PR status")
                            .on_press(Message::FetchGitHubPullRequestStatus)
                            .style(iced::widget::button::secondary),
                    );
                }
            }
            if let Some(path) = &session.worktree_path {
                detail = detail.push(text(format!("worktree: {}", path.display())).size(14));
            }
            detail = detail.push(text(format!("cwd: {}", session.cwd.display())).size(14));
            detail = detail.push(
                text(format!(
                    "cwd source: {}",
                    cwd_source_label(session.cwd_source)
                ))
                .size(14),
            );
            if has_worktree_drift(session) {
                detail = detail.push(text("worktree drift: yes").size(14));
            }
            detail = detail.push(session_actions(app, host_id, session, external));
            if let Some(observation) = session_observation_view(app, host_id, session) {
                detail = detail.push(observation);
            }
            if !external {
                detail = detail.push(rename_view(app));
                detail = detail.push(metadata_view(app, session));
            }
        }
        None => {
            detail = detail.push(text("No session selected").size(16));
        }
    }
    card(detail)
}

fn session_observation_view(
    app: &PohunekApp,
    host_id: &HostId,
    session: &SessionInfo,
) -> Option<Element<'static, Message>> {
    let observation = app.workspace.session_observation(host_id, &session.id)?;
    let mut content = column![].spacing(4);
    if let Some(screen) = &observation.screen {
        content = content
            .push(text("Terminal screen").size(14))
            .push(text(screen.visible_lines.join("\n")).size(12));
    }
    if let Some((start, end)) = observation.output_gap {
        content = content.push(
            text(format!(
                "output gap: {start}..{end}; showing retained bytes"
            ))
            .size(12),
        );
    }
    if !observation.output_text.is_empty() {
        content = content
            .push(text("Retained output").size(14))
            .push(text(observation.output_text.clone()).size(12));
    }
    if let Some(wait) = &observation.wait {
        content =
            content.push(text(format!("last wait: {}", wait_reason_label(wait.reason))).size(12));
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
        let needs_resume = session_requires_resume_before_attach(app, host_id, &session.id);
        let can_open = session_can_open(session);
        let open_label = if needs_resume && !can_open {
            "Resume unavailable"
        } else {
            "Open in terminal"
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
            ))
            .push(
                button("Stop")
                    .on_press(Message::StopSelectedSession)
                    .style(iced::widget::button::danger),
            )
            .push(
                button("Remove")
                    .on_press(Message::RemoveSelectedSession)
                    .style(iced::widget::button::danger),
            );
        if session.worktree_path.is_some() {
            actions = actions.push(
                button("Review changes")
                    .on_press(Message::OpenSessionReview {
                        host_id: host_id.clone(),
                        session_id: session.id.clone(),
                    })
                    .style(iced::widget::button::secondary),
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
            metadata = metadata.push(text(format!("{} = {}", row.key, row.value)).size(13));
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
        text_input("optional session name", &app.start.name).on_input(Message::StartNameChanged),
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
