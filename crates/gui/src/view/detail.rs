//! Detail-pane routing: start-work landing, host, and host-scoped project views.

use iced::widget::{button, column, row, scrollable, text, text_input};
use iced::Element;
use pohunek_gui_core::{HostId, Selection};

use crate::message::Message;
use crate::selection::project_is_selected;
use crate::view::inbox::{inbox_pane, notification_pane};
use crate::view::modals::toast_view;
use crate::view::project::project_pane;
use crate::view::session::session_pane;
use crate::view::tree::conn_label;
use crate::PohunekApp;

use super::{card, list_button, section_title};

/// Routes the detail pane to the surface that matches the current selection,
/// instead of stacking every form unconditionally. Sessions show a session
/// card; projects show the project plus its start/provider/action surfaces;
/// hosts show project management; nothing selected shows a start-work landing.
pub(crate) fn detail_view(app: &PohunekApp) -> Element<'_, Message> {
    let body = match app.ui_state.selection.as_ref() {
        Some(Selection::Session { .. }) => session_pane(app),
        Some(Selection::Project { .. }) => project_pane(app),
        Some(Selection::Host { host_id }) => host_pane(app, host_id),
        Some(Selection::Notification { .. }) => notification_pane(app),
        None if app.inbox_open => inbox_pane(app),
        None => start_work_pane(app),
    };
    let mut detail = column![body].spacing(12);
    for toast in app.workspace.toasts.iter().rev().take(3).rev() {
        detail = detail.push(toast_view(toast));
    }
    if let Some(status) = &app.status {
        detail = detail.push(text(status).size(13));
    }
    scrollable(detail).into()
}

/// Landing surface shown when nothing is selected: a guided entry point that
/// lets the operator jump straight into any known project rather than facing an
/// empty form.
fn start_work_pane(app: &PohunekApp) -> Element<'_, Message> {
    let mut projects = column![].spacing(4);
    let mut any_project = false;
    for (host_id, host) in &app.workspace.hosts {
        for project in host.projects.values() {
            any_project = true;
            projects = projects.push(list_button(
                text(format!("{}   ·   {host_id}", project.label)).size(15),
                Message::SelectProject {
                    host_id: host_id.clone(),
                    project_id: project.id.clone(),
                },
                false,
            ));
        }
    }
    if !any_project {
        projects = projects.push(
            text("No projects yet. Select a host in the workspace tree to add one.").size(13),
        );
    }
    column![
        text("Start work").size(22),
        text("Pick a project to start an agent, browse Linear issues, or open a pull request.")
            .size(14),
        card(projects),
    ]
    .spacing(12)
    .into()
}

/// Host surface: connection summary plus project management for that host.
fn host_pane<'a>(app: &'a PohunekApp, host_id: &'a HostId) -> Element<'a, Message> {
    let conn = app
        .workspace
        .hosts
        .get(host_id)
        .map_or("unknown", |host| conn_label(&host.conn));
    column![
        text(format!("Host {host_id}")).size(22),
        text(format!("connection: {conn}")).size(14),
        host_projects_view(app, host_id),
    ]
    .spacing(12)
    .into()
}

/// Host-scoped project surface: the host's registered projects (each selectable)
/// plus an "Add project" form. Rename/remove live in the project surface, scoped
/// to the selected project, instead of a generic reference field here.
fn host_projects_view<'a>(app: &'a PohunekApp, host_id: &'a HostId) -> Element<'a, Message> {
    let mut view = column![section_title("Projects")].spacing(8);
    match app.workspace.hosts.get(host_id) {
        Some(host) if !host.projects.is_empty() => {
            for project in host.projects.values() {
                view = view.push(list_button(
                    text(format!("{}   ({})", project.label, project.id)).size(14),
                    Message::SelectProject {
                        host_id: host_id.clone(),
                        project_id: project.id.clone(),
                    },
                    project_is_selected(app, host_id, &project.id),
                ));
            }
        }
        _ => view = view.push(text("No projects registered on this host").size(13)),
    }
    let view = view.push(text("Add project").size(15)).push(
        row![
            text_input("path", &app.project_edit.path).on_input(Message::ProjectPathChanged),
            text_input("name (optional)", &app.project_edit.name)
                .on_input(Message::ProjectNameChanged),
            text_input("base branch (optional)", &app.project_edit.base_branch)
                .on_input(Message::ProjectBaseBranchChanged),
            button("Add")
                .on_press(Message::AddProject)
                .style(iced::widget::button::secondary),
        ]
        .spacing(8),
    );
    card(view)
}
