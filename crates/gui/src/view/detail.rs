//! Prioritized session list for the native GUI workspace.

// Rust guideline compliant 2026-08-12

use iced::widget::{button, column, container, row, scrollable, text};
use iced::{Center, Element, Fill, Theme};
use pohunek_gui_core::{SessionAccess, SessionGroup, SessionRow};
use protocol::AgentActivity;

use crate::message::Message;
use crate::selection::selected_project;
use crate::view::modals::toast_view;
use crate::PohunekApp;

use super::{card, list_button, push_meta, STATUS_DOT};

/// Renders the primary session-first workspace.
pub(crate) fn detail_view(app: &PohunekApp) -> Element<'_, Message> {
    let rows = app.workspace.session_rows();
    let mut content = column![session_header(app)].spacing(12);
    for group in [
        SessionGroup::NeedsAction,
        SessionGroup::Idle,
        SessionGroup::Running,
        SessionGroup::Unavailable,
    ] {
        content = content.push(session_group(group, &rows));
    }
    for toast in app.workspace.toasts.iter().rev().take(3).rev() {
        content = content.push(toast_view(toast));
    }
    if let Some(status) = &app.status {
        content = content.push(text(status).size(13));
    }
    scrollable(content).into()
}

fn session_header(app: &PohunekApp) -> Element<'_, Message> {
    let (new_session, context) = if let Some((host_id, project)) = selected_project(app) {
        (
            button("New session")
                .on_press(Message::OpenStartModal)
                .style(iced::widget::button::primary),
            Some(format!(
                "New sessions start in {host_id} / {}",
                project.label
            )),
        )
    } else {
        (
            button("New session").style(iced::widget::button::primary),
            None,
        )
    };
    row![
        column![
            text("Sessions").size(24),
            text(context.unwrap_or_else(|| {
                "Select a project on the left to start a session".to_owned()
            }))
            .size(13)
        ]
        .spacing(3),
        iced::widget::space().width(Fill),
        new_session,
    ]
    .align_y(Center)
    .into()
}

fn session_group(group: SessionGroup, rows: &[SessionRow]) -> Element<'static, Message> {
    let matching: Vec<&SessionRow> = rows.iter().filter(|row| row.group == group).collect();
    let count = matching.len();
    let mut list = column![row![
        text(group_label(group)).size(18),
        text(count.to_string()).size(13),
    ]
    .spacing(8)
    .align_y(Center)]
    .spacing(6);
    if matching.is_empty() {
        list = list.push(text(group_empty_label(group)).size(13));
    } else {
        for session in matching {
            list = list.push(session_row(session));
        }
    }
    card(list)
}

fn session_row(row: &SessionRow) -> Element<'static, Message> {
    let title = row.name.clone().unwrap_or_else(|| row.session_id.0.clone());
    let mut metadata = format!("{}  ·  {}", row.host_id, row.agent);
    if let Some(project) = row.project_label.as_ref().or(row.project_id.as_ref()) {
        push_meta(&mut metadata, project);
    }
    if let Some(branch) = &row.branch {
        push_meta(&mut metadata, branch);
    }
    push_meta(&mut metadata, row.state.as_str());
    if let Some(activity) = row.activity {
        push_meta(&mut metadata, activity.as_str());
    }

    let target_host = row.host_id.clone();
    let target_session = row.session_id.clone();
    let info = list_button(
        column![text(title).size(15), text(metadata).size(12)].spacing(2),
        Message::SelectSession {
            host_id: target_host.clone(),
            session_id: target_session.clone(),
        },
        false,
    );
    let mut actions = row![].spacing(6).align_y(Center);
    match row.access {
        SessionAccess::Attach | SessionAccess::Resume => {
            let label = if row.access == SessionAccess::Resume {
                "Resume"
            } else {
                "Open"
            };
            actions = actions.push(
                button(text(label).size(12))
                    .padding([5, 9])
                    .on_press(Message::OpenSession {
                        host_id: target_host.clone(),
                        session_id: target_session.clone(),
                    })
                    .style(iced::widget::button::primary),
            );
        }
        SessionAccess::Pending => {
            actions = actions.push(
                button(text("Pending").size(12))
                    .padding([5, 9])
                    .style(iced::widget::button::secondary),
            );
        }
        SessionAccess::Unavailable => {}
    }
    if row.can_stop {
        actions = actions.push(
            button(text("Terminate").size(12))
                .padding([5, 9])
                .on_press(Message::StopSession {
                    host_id: target_host.clone(),
                    session_id: target_session.clone(),
                })
                .style(iced::widget::button::danger),
        );
    }
    if row.can_remove {
        actions = actions.push(
            button(text("Delete").size(12))
                .padding([5, 9])
                .on_press(Message::RequestDeleteSession {
                    host_id: target_host,
                    session_id: target_session,
                })
                .style(iced::widget::button::danger),
        );
    }

    container(
        row![session_dot(row.activity), info, actions]
            .spacing(8)
            .align_y(Center),
    )
    .padding([3, 0])
    .width(Fill)
    .into()
}

fn group_label(group: SessionGroup) -> &'static str {
    match group {
        SessionGroup::NeedsAction => "Needs action",
        SessionGroup::Idle => "Idle",
        SessionGroup::Running => "Running",
        SessionGroup::Unavailable => "Unavailable",
    }
}

fn group_empty_label(group: SessionGroup) -> &'static str {
    match group {
        SessionGroup::NeedsAction => "Nothing needs your attention.",
        SessionGroup::Idle => "No idle attachable sessions.",
        SessionGroup::Running => "No sessions are currently working or starting.",
        SessionGroup::Unavailable => "No unavailable sessions.",
    }
}

fn session_dot(activity: Option<AgentActivity>) -> Element<'static, Message> {
    text(STATUS_DOT)
        .size(13)
        .style(move |theme: &Theme| {
            let palette = theme.extended_palette();
            let color = match activity {
                Some(AgentActivity::Working) => palette.success.base.color,
                Some(AgentActivity::Blocked) => palette.danger.base.color,
                Some(AgentActivity::Idle) => palette.secondary.base.color,
                None => palette.background.strong.color,
            };
            iced::widget::text::Style { color: Some(color) }
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_labels_match_priority_sections() {
        assert_eq!(group_label(SessionGroup::NeedsAction), "Needs action");
        assert_eq!(group_label(SessionGroup::Idle), "Idle");
        assert_eq!(group_label(SessionGroup::Running), "Running");
        assert_eq!(group_label(SessionGroup::Unavailable), "Unavailable");
    }

    #[test]
    fn metadata_builder_uses_stable_separator() {
        let mut metadata = "local".to_owned();
        push_meta(&mut metadata, "project");
        push_meta(&mut metadata, "idle");
        assert_eq!(metadata, "local  ·  project  ·  idle");
    }
}
