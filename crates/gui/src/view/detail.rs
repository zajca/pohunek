//! Right-pane tab bar and per-tab body routing.
//!
//! The right pane is a persistent `Detail · Linear · GitHub · Worktrees` tab
//! strip over a body that switches with `app.ui_state.active_tab`. Detail
//! keeps the old selection-driven routing (session/project/host/start-work
//! landing); the other three promote `linear_provider_view`,
//! `github_provider_view`, and `project_worktrees` — previously stacked
//! inside `project_pane` — to full tab bodies scoped to the current project.
//! Selecting a session anywhere (`command::update`) force-switches back to
//! Detail so triage never lands behind a Linear/GitHub/Worktrees tab.

use iced::widget::{button, column, container, row, scrollable, text, text_input, tooltip};
use iced::{Center, Element, Fill};
use pohunek_gui_core::{HostId, RightTab, Selection};

use crate::message::Message;
use crate::selection::{
    effective_filters, project_is_selected, selected_github_scope, selected_host_id,
    tab_project_scope,
};
use crate::view::modals::toast_view;
use crate::view::project::{project_pane, project_worktrees};
use crate::view::provider::{github_provider_view, linear_provider_view};
use crate::view::session::session_pane;
use crate::view::tree::conn_label;
use crate::PohunekApp;

use super::{card, conn_dot, list_button, section_title};

/// Right pane: the persistent tab bar, the active tab's body, toasts, and the
/// status line.
pub(crate) fn detail_view(app: &PohunekApp) -> Element<'_, Message> {
    let mut detail = column![tab_bar(app), tab_body(app)].spacing(12);
    for toast in app.workspace.toasts.iter().rev().take(3).rev() {
        detail = detail.push(toast_view(toast));
    }
    if let Some(status) = &app.status {
        detail = detail.push(text(status).size(13));
    }
    scrollable(detail).into()
}

/// The `1 Detail · 2 Linear · 3 GitHub · 4 Worktrees` tab strip, with a
/// trailing context chip showing the project scope tabs 2-4 operate on.
fn tab_bar(app: &PohunekApp) -> Element<'_, Message> {
    let scope = tab_project_scope(app);
    let bar = row![
        tab_button(app, RightTab::Detail, "1 Detail", true),
        tab_button(app, RightTab::Linear, "2 Linear", scope.is_some()),
        tab_button(app, RightTab::GitHub, "3 GitHub", scope.is_some()),
        tab_button(app, RightTab::Worktrees, "4 Worktrees", scope.is_some()),
    ]
    .spacing(6)
    .align_y(Center)
    .push(iced::widget::space().width(Fill))
    .push(context_chip(app, scope));
    bar.into()
}

/// One tab button: primary when active, secondary otherwise. Disabled tabs
/// (no project scope) render with no `on_press` — Iced's default button
/// styles dim a pressless button automatically — and a "Select a project"
/// tooltip in place of the click.
fn tab_button(
    app: &PohunekApp,
    tab: RightTab,
    label: &'static str,
    enabled: bool,
) -> Element<'static, Message> {
    let style = if app.ui_state.active_tab == tab {
        iced::widget::button::primary
    } else {
        iced::widget::button::secondary
    };
    let button = button(text(label).size(14)).style(style);
    if enabled {
        button.on_press(Message::SelectTab(tab)).into()
    } else {
        tooltip(
            button,
            container(text("Select a project").size(12))
                .padding(6)
                .style(iced::widget::container::rounded_box),
            tooltip::Position::Bottom,
        )
        .into()
    }
}

/// Right end of the tab strip: `host / project-label` when tabs 2-4 have a
/// project scope, otherwise just the (fallback-resolved) host's connection
/// dot, matching how `provider_browser_view` used to derive scope before B2.
fn context_chip(app: &PohunekApp, scope: Option<(HostId, String)>) -> Element<'_, Message> {
    let (host_id, label) = match scope {
        Some((host_id, label)) => (host_id, Some(label)),
        None => match selected_host_id(app) {
            Ok(host_id) => (host_id, None),
            Err(_) => return row![].into(),
        },
    };
    let mut chip = row![].spacing(6).align_y(Center);
    if let Some(host) = app.workspace.hosts.get(&host_id) {
        chip = chip.push(conn_dot(host.conn.clone()));
    }
    let caption = match label {
        Some(label) => format!("{host_id} / {label}"),
        None => host_id.to_string(),
    };
    chip.push(text(caption).size(13)).into()
}

/// Routes the tab body to the surface for `app.ui_state.active_tab`.
fn tab_body(app: &PohunekApp) -> Element<'_, Message> {
    match app.ui_state.active_tab {
        RightTab::Detail => detail_body(app),
        RightTab::Linear => linear_tab_body(app),
        RightTab::GitHub => github_tab_body(app),
        RightTab::Worktrees => worktrees_tab_body(app),
    }
}

/// Detail tab body: routes by selection, exactly as the whole right pane did
/// before the tab bar existed. Sessions show a session card; projects show
/// identity/rename/New-session; hosts show project management; nothing
/// selected shows a start-work landing. Notifications have no selection
/// route: the inbox modal (`ModalView::Inbox`) is their only surface.
fn detail_body(app: &PohunekApp) -> Element<'_, Message> {
    match app.ui_state.selection.as_ref() {
        Some(Selection::Session { .. }) => session_pane(app),
        Some(Selection::Project { .. }) => project_pane(app),
        Some(Selection::Host { host_id }) => host_pane(app, host_id),
        None => start_work_pane(app),
    }
}

/// Empty state for a project-scoped tab (Linear/GitHub/Worktrees) rendered
/// when [`tab_project_scope`] finds none — reachable even though the tab
/// button itself is disabled in that state, since switching selection away
/// from a project while the tab is already active must not panic or stall.
fn project_scope_placeholder() -> Element<'static, Message> {
    card(text("Select a project to view this tab.").size(13))
}

/// Linear tab body: the Linear issue browser scoped to the current project.
fn linear_tab_body(app: &PohunekApp) -> Element<'_, Message> {
    let Some((host_id, _)) = tab_project_scope(app) else {
        return project_scope_placeholder();
    };
    let Some(host) = app.workspace.hosts.get(&host_id) else {
        return card(text("Host is not loaded").size(13));
    };
    let filters = effective_filters(app);
    card(linear_provider_view(host_id, host, filters.linear_names()))
}

/// GitHub tab body: the GitHub PR/issue browser scoped to the current project.
fn github_tab_body(app: &PohunekApp) -> Element<'_, Message> {
    let Some((host_id, _)) = tab_project_scope(app) else {
        return project_scope_placeholder();
    };
    let Some(host) = app.workspace.hosts.get(&host_id) else {
        return card(text("Host is not loaded").size(13));
    };
    let current_scope = selected_github_scope(app).ok();
    let filters = effective_filters(app);
    card(github_provider_view(
        host_id,
        current_scope,
        host,
        filters.github_names(),
    ))
}

/// Worktrees tab body: the current project's worktree list.
fn worktrees_tab_body(app: &PohunekApp) -> Element<'_, Message> {
    if tab_project_scope(app).is_none() {
        return project_scope_placeholder();
    }
    project_worktrees(app)
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
