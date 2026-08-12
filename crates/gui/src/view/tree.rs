//! Host and project navigation for the native GUI.

// Rust guideline compliant 2026-08-12

use std::collections::BTreeSet;

use iced::widget::{button, column, row, scrollable, text};
use iced::{Center, Element, Fill, Theme};
use pohunek_gui_core::{ConnState, HostId, TreeNodeId};
use protocol::ProjectInfo;

use crate::message::{Message, ModalView};
use crate::selection::project_is_selected;
use crate::PohunekApp;

use super::{caret, conn_dot, indent, list_button};

pub(crate) fn inbox_entry_button(app: &PohunekApp) -> Element<'_, Message> {
    let unread = app.workspace.unread_notification_count();
    let label = if unread == 0 {
        "Inbox".to_owned()
    } else {
        format!("Inbox {unread}")
    };
    let button = button(text(label).size(14))
        .width(Fill)
        .padding([8, 10])
        .on_press(Message::OpenInbox);
    if app.modal == ModalView::Inbox {
        button.style(iced::widget::button::primary).into()
    } else {
        button.style(iced::widget::button::secondary).into()
    }
}

pub(crate) fn assistant_entry_button() -> Element<'static, Message> {
    button(
        row![text("◎").size(14), text("Assistant").size(14)]
            .spacing(6)
            .align_y(Center),
    )
    .width(Fill)
    .padding([8, 10])
    .on_press(Message::OpenAssistantModal)
    .style(iced::widget::button::primary)
    .into()
}

pub(crate) fn workspace_tree(app: &PohunekApp) -> Element<'_, Message> {
    let mut tree = column![text("Projects").size(16)].spacing(4);
    if let Err(err) = &app.config {
        tree = tree.push(text(format!("configuration error: {err}")).size(14));
        return scrollable(tree).into();
    }
    for (host_id, host) in &app.workspace.hosts {
        let node = TreeNodeId::host(host_id.clone());
        let expanded = app.ui_state.expanded_nodes.contains(&node);
        let unread = app.workspace.host_unread_notification_count(host_id);
        let mut host_row = row![
            caret(expanded, node),
            conn_dot(host.conn.clone()),
            text(host_id.to_string()).size(15)
        ]
        .spacing(6)
        .align_y(Center);
        if unread > 0 {
            host_row = host_row.push(
                button(text(format!("inbox {unread}")).size(12))
                    .padding([2, 6])
                    .on_press(Message::OpenHostInbox(host_id.clone()))
                    .style(iced::widget::button::text),
            );
        }
        tree = tree.push(host_row);
        if let Some(error) = &host.last_error {
            tree = tree.push(indent(1, text(error).size(12)));
        }
        if expanded {
            tree = push_project_rows(tree, app, host_id, host);
        }
    }
    if app.workspace.hosts.is_empty() {
        tree = tree.push(text("connecting…").size(13));
    }
    scrollable(tree).into()
}

fn push_project_rows<'a>(
    mut tree: iced::widget::Column<'a, Message>,
    app: &'a PohunekApp,
    host_id: &'a HostId,
    host: &'a pohunek_gui_core::HostView,
) -> iced::widget::Column<'a, Message> {
    for project in host.projects.values() {
        tree = tree.push(project_row(app, host_id, project));
    }
    let missing_project_ids = host
        .sessions
        .values()
        .filter_map(|session| {
            let project_id = session.project_id.as_ref()?;
            (!host.projects.contains_key(project_id)).then(|| project_id.clone())
        })
        .collect::<BTreeSet<_>>();
    for project_id in missing_project_ids {
        tree = tree.push(missing_project_row(app, host_id, &project_id));
    }
    tree
}

fn project_row(
    app: &PohunekApp,
    host_id: &HostId,
    project: &ProjectInfo,
) -> Element<'static, Message> {
    indent(
        1,
        list_button(
            text(project.label.clone()).size(14),
            Message::SelectProject {
                host_id: host_id.clone(),
                project_id: project.id.clone(),
            },
            project_is_selected(app, host_id, &project.id),
        ),
    )
}

fn missing_project_row(
    app: &PohunekApp,
    host_id: &HostId,
    project_id: &str,
) -> Element<'static, Message> {
    indent(
        1,
        list_button(
            text(format!("Unknown project {project_id}")).size(14),
            Message::SelectProject {
                host_id: host_id.clone(),
                project_id: project_id.to_owned(),
            },
            project_is_selected(app, host_id, project_id),
        ),
    )
}

pub(crate) fn conn_color(theme: &Theme, conn: &ConnState) -> iced::Color {
    let palette = theme.extended_palette();
    match conn {
        ConnState::Connected => palette.success.base.color,
        ConnState::Connecting => palette.warning.base.color,
        ConnState::Disconnected | ConnState::Unreachable => palette.danger.base.color,
    }
}
