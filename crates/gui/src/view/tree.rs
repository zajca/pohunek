//! Workspace tree, agents monitor, and connection/activity status indicators.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use iced::widget::{button, column, row, scrollable, text};
use iced::{Center, Element, Fill, Theme};
use pohunek_gui_core::{ConnState, HostId, TreeNodeId};
use protocol::{AgentActivity, ProjectInfo, SessionInfo};

use crate::message::{Message, ModalView};
use crate::selection::{project_is_selected, session_is_selected};
use crate::view::provider::linked_pr_status_label;
use crate::PohunekApp;

use super::{caret, conn_dot, indent, list_button, push_meta, session_agent_label, STATUS_DOT};

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
    let mut tree = column![text("Workspace").size(16)].spacing(4);
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
        tree = push_project_row(tree, app, host_id, host, Some(project));
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
        tree = push_missing_project_row(tree, app, host_id, host, &project_id);
    }
    if host
        .sessions
        .values()
        .any(|session| session.project_id.is_none())
    {
        tree = push_project_row(tree, app, host_id, host, None);
    }
    tree
}

fn push_missing_project_row<'a>(
    mut tree: iced::widget::Column<'a, Message>,
    app: &'a PohunekApp,
    host_id: &'a HostId,
    host: &'a pohunek_gui_core::HostView,
    project_id: &str,
) -> iced::widget::Column<'a, Message> {
    let node = TreeNodeId::project(host_id.clone(), project_id);
    let expanded = app.ui_state.expanded_nodes.contains(&node);
    let selected = project_is_selected(app, host_id, project_id);
    tree = tree.push(indent(
        1,
        row![
            caret(expanded, node),
            list_button(
                text(format!("Unknown project {project_id}")).size(14),
                Message::SelectProject {
                    host_id: host_id.clone(),
                    project_id: project_id.to_owned(),
                },
                selected,
            ),
        ]
        .spacing(4)
        .align_y(Center),
    ));
    if expanded {
        for session in host
            .sessions
            .values()
            .filter(|session| session.project_id.as_deref() == Some(project_id))
        {
            tree = tree.push(session_tree_row(app, host_id, host, session));
        }
    }
    tree
}

fn push_project_row<'a>(
    mut tree: iced::widget::Column<'a, Message>,
    app: &'a PohunekApp,
    host_id: &'a HostId,
    host: &'a pohunek_gui_core::HostView,
    project: Option<&'a ProjectInfo>,
) -> iced::widget::Column<'a, Message> {
    let project_id = project.map_or_else(|| "unassigned".to_owned(), |project| project.id.clone());
    let label = project.map_or("No project", |project| project.label.as_str());
    let node = TreeNodeId::project(host_id.clone(), project_id.clone());
    let expanded = app.ui_state.expanded_nodes.contains(&node);
    let selected = project_is_selected(app, host_id, &project_id);
    tree = tree.push(indent(
        1,
        row![
            caret(expanded, node),
            list_button(
                text(label).size(14),
                Message::SelectProject {
                    host_id: host_id.clone(),
                    project_id,
                },
                selected,
            ),
        ]
        .spacing(4)
        .align_y(Center),
    ));
    if expanded {
        for session in host.sessions.values().filter(|session| {
            project.map_or_else(
                || session.project_id.is_none(),
                |project| session.project_id.as_deref() == Some(project.id.as_str()),
            )
        }) {
            tree = tree.push(session_tree_row(app, host_id, host, session));
        }
    }
    tree
}

fn session_tree_row(
    app: &PohunekApp,
    host_id: &HostId,
    host: &pohunek_gui_core::HostView,
    session: &SessionInfo,
) -> Element<'static, Message> {
    let provider_status = linked_pr_status_label(host, session);
    let runtime_status = session.runtime.as_ref().map_or("", |runtime| {
        if runtime.state == protocol::RuntimeState::Live {
            ""
        } else {
            runtime.state.as_str()
        }
    });
    let runtime_status = if runtime_status.is_empty() {
        String::new()
    } else {
        format!("  runtime:{runtime_status}")
    };
    let selected = session_is_selected(app, host_id, &session.id);
    let origin = if session.external == Some(true) {
        "  external"
    } else {
        ""
    };
    // Lead with the display name when set; otherwise fall back to the id.
    let label = match &session.name {
        Some(name) => format!(
            "{name}  {}{origin}{provider_status}{runtime_status}",
            session_agent_label(session)
        ),
        None => format!(
            "{}  {}{origin}{provider_status}{runtime_status}",
            session.id.0,
            session_agent_label(session)
        ),
    };
    indent(
        2,
        row![
            status_dot(session.activity),
            list_button(
                text(label).size(14),
                Message::SelectSession {
                    host_id: host_id.clone(),
                    session_id: session.id.clone(),
                },
                selected,
            ),
        ]
        .spacing(6)
        .align_y(Center),
    )
}

pub(crate) fn agents_monitor(app: &PohunekApp) -> Element<'_, Message> {
    let monitor = app.workspace.agent_monitor();
    let filter = app.activity_filter;
    let header = row![
        text("Agents").size(18),
        activity_chip("working", AgentActivity::Working, monitor.working, filter),
        activity_chip("blocked", AgentActivity::Blocked, monitor.blocked, filter),
        activity_chip("idle", AgentActivity::Idle, monitor.idle, filter),
        text(format!("unknown {}", monitor.unknown)).size(13),
    ]
    .spacing(8);
    let mut list = column![header].spacing(5);
    let mut shown = 0_usize;
    for agent in monitor.sessions {
        if filter.is_some() && agent.activity != filter {
            continue;
        }
        shown += 1;
        let selected = session_is_selected(app, &agent.host_id, &agent.session_id);
        // Primary line leads with the display name when set, else the id.
        let primary = match &agent.name {
            Some(name) => format!("{name}  ·  {}", agent.agent),
            None => format!(
                "{} / {}  ·  {}",
                agent.host_id, agent.session_id.0, agent.agent
            ),
        };
        // Secondary line packs the context that was previously missing: host (when
        // a name hid it), project, branch, and the activity word.
        let mut meta = String::new();
        if agent.name.is_some() {
            let _ = write!(&mut meta, "{} / {}", agent.host_id, agent.session_id.0);
        }
        if let Some(project) = agent.project_label.as_ref().or(agent.project_id.as_ref()) {
            push_meta(&mut meta, project);
        }
        if let Some(branch) = &agent.branch {
            push_meta(&mut meta, branch);
        }
        push_meta(
            &mut meta,
            agent.activity.map_or("unknown", AgentActivity::as_str),
        );
        list = list.push(
            row![
                status_dot(agent.activity),
                list_button(
                    column![text(primary).size(14), text(meta).size(11)].spacing(1),
                    Message::SelectSession {
                        host_id: agent.host_id,
                        session_id: agent.session_id,
                    },
                    selected,
                ),
            ]
            .spacing(6)
            .align_y(Center),
        );
    }
    if shown == 0 {
        let empty = if filter.is_some() {
            "No agents match the filter"
        } else {
            "No agents"
        };
        list = list.push(text(empty).size(13));
    }
    scrollable(list).into()
}

/// A clickable activity count chip for the agents monitor. Clicking toggles the
/// monitor's activity filter: selecting an already-active activity clears it.
fn activity_chip(
    label: &str,
    activity: AgentActivity,
    count: usize,
    filter: Option<AgentActivity>,
) -> Element<'static, Message> {
    let active = filter == Some(activity);
    let target = if active { None } else { Some(activity) };
    let content = row![
        status_dot(Some(activity)),
        text(format!("{label} {count}")).size(13)
    ]
    .spacing(4);
    let chip = button(content).on_press(Message::FilterActivity(target));
    if active {
        chip.style(iced::widget::button::primary).into()
    } else {
        chip.style(iced::widget::button::text).into()
    }
}

pub(crate) fn conn_label(conn: &ConnState) -> &'static str {
    match conn {
        ConnState::Connecting => "connecting",
        ConnState::Connected => "connected",
        ConnState::Disconnected => "disconnected",
        ConnState::Unreachable => "unreachable",
    }
}

/// Themed color for an agent-activity status dot: working=success (green),
/// blocked=danger (red), idle=secondary (muted), unknown=dim background.
fn activity_color(theme: &Theme, activity: Option<AgentActivity>) -> iced::Color {
    let palette = theme.extended_palette();
    match activity {
        Some(AgentActivity::Working) => palette.success.base.color,
        Some(AgentActivity::Blocked) => palette.danger.base.color,
        Some(AgentActivity::Idle) => palette.secondary.base.color,
        None => palette.background.strong.color,
    }
}

/// A filled-circle indicator colored by agent activity.
fn status_dot(activity: Option<AgentActivity>) -> Element<'static, Message> {
    text(STATUS_DOT)
        .size(13)
        .style(move |theme: &Theme| iced::widget::text::Style {
            color: Some(activity_color(theme, activity)),
        })
        .into()
}

/// Themed color for a host connection dot: connected=success, connecting=warning,
/// disconnected/unreachable=danger.
pub(crate) fn conn_color(theme: &Theme, conn: &ConnState) -> iced::Color {
    let palette = theme.extended_palette();
    match conn {
        ConnState::Connected => palette.success.base.color,
        ConnState::Connecting => palette.warning.base.color,
        ConnState::Disconnected | ConnState::Unreachable => palette.danger.base.color,
    }
}
