//! Session detail pane: summary, rename control, and metadata editor.

use iced::widget::{button, column, row, text, text_input};
use iced::{Center, Element};
use pohunek_gui_core::{
    session_link_metadata, session_metadata_rows, SessionLinkKind, SessionLinkProvider,
};
use protocol::SessionInfo;

use crate::message::Message;
use crate::selection::{selected_host_config, selected_session};
use crate::view::provider::linked_github_status;
use crate::PohunekApp;

use super::{card, section_title};

/// Session surface: the session card with its actions and metadata.
pub(crate) fn session_pane(app: &PohunekApp) -> Element<'_, Message> {
    session_detail(app)
}

fn session_detail(app: &PohunekApp) -> Element<'_, Message> {
    let mut detail = column![section_title("Session")].spacing(8);
    match selected_session(app) {
        Some((host_id, session)) => {
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
                .push(text(format!("agent: {}", session.agent)).size(14))
                .push(text(format!("state: {}", session.state.as_str())).size(14))
                .push(text(format!("activity: {activity}")).size(14));
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
                row![
                    button("Open in terminal")
                        .on_press(Message::OpenSession {
                            host_id: host_id.clone(),
                            session_id: session.id.clone(),
                        })
                        .style(iced::widget::button::primary),
                    button("Inspect")
                        .on_press(Message::InspectSelectedSession)
                        .style(iced::widget::button::secondary),
                    button("Stop")
                        .on_press(Message::StopSelectedSession)
                        .style(iced::widget::button::danger),
                    button("Remove")
                        .on_press(Message::RemoveSelectedSession)
                        .style(iced::widget::button::danger)
                ]
                .spacing(8),
            );
            detail = detail.push(rename_view(app));
            detail = detail.push(metadata_view(app, session));
        }
        None => {
            detail = detail.push(text("No session selected").size(16));
        }
    }
    card(detail)
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
