//! Single-pane session overview for the native GUI workspace.

// Rust guideline compliant 2026-10-01

use std::collections::BTreeMap;

use iced::widget::{button, column, container, row, scrollable, text};
use iced::{Background, Center, Element, Fill, Theme};
use pohunek_gui_core::{ProjectChoice, ProjectRef, SessionAccess, SessionGroup, SessionRow};
use protocol::{AgentActivity, NotificationKind};

use crate::message::Message;
use crate::view::hosts::hosts_button;
use crate::view::inbox::notification_age_label;
use crate::view::modals::toast_view;
use crate::PohunekApp;

use super::{card, list_button, muted_style, push_meta, status_pill, PillTone, STATUS_DOT};

/// Heading size of the overview title.
const TITLE_SIZE: u32 = 24;

/// Vertical gap between the overview's top-level blocks.
const PANE_SPACING: u32 = 12;

/// Session name size in a row.
const ROW_TITLE_SIZE: u32 = 16;

/// Size of the branch and project-chip text in a row.
const ROW_DETAIL_SIZE: u32 = 13;

/// Size of the muted host / agent / state line in a row.
const ROW_META_SIZE: u32 = 12;

/// Corner radius of project chips and filter chips.
const CHIP_RADIUS: f32 = 4.0;

/// Maximum number of recent toasts shown below the list.
const VISIBLE_TOASTS: usize = 3;

/// Label of the filter chip that clears the project filter.
const ALL_PROJECTS_LABEL: &str = "All";

/// Label of the project chip on a session that has no project.
const NO_PROJECT_LABEL: &str = "no project";

/// Renders the session overview: header, project filter, and grouped sessions.
pub(crate) fn detail_view(app: &PohunekApp) -> Element<'_, Message> {
    let filter = app.project_filter.as_ref();
    let rows = app.workspace.session_rows_filtered(filter);
    let mut content = column![session_header(app)].spacing(PANE_SPACING);
    if let Err(err) = &app.config {
        content = content.push(config_error_banner(err));
    }
    let filters = app.workspace.session_project_filters();
    if !filters.is_empty() {
        let total = app
            .workspace
            .hosts
            .values()
            .map(|host| host.sessions.len())
            .sum();
        content = content.push(project_filter_row(&filters, total, filter));
    }
    if rows.is_empty() {
        content = content.push(
            text(empty_label(app))
                .size(ROW_DETAIL_SIZE)
                .style(muted_style),
        );
    }
    for group in [
        SessionGroup::NeedsYou,
        SessionGroup::Running,
        SessionGroup::Ready,
        SessionGroup::Unavailable,
    ] {
        if rows.iter().any(|row| row.group == group) {
            content = content.push(session_group(group, &rows));
        }
    }
    for toast in app.workspace.toasts.iter().rev().take(VISIBLE_TOASTS).rev() {
        content = content.push(toast_view(toast));
    }
    if let Some(status) = &app.status {
        content = content.push(text(status).size(ROW_DETAIL_SIZE));
    }
    scrollable(content).into()
}

fn empty_label(app: &PohunekApp) -> &'static str {
    if app.workspace.project_choices().is_empty() {
        "No projects are available yet. Open Hosts to check the host connections."
    } else {
        "No sessions yet. Choose New session to start one."
    }
}

fn session_header(app: &PohunekApp) -> Element<'_, Message> {
    let mut new_session = button("New session").style(iced::widget::button::primary);
    if !app.workspace.project_choices().is_empty() {
        new_session = new_session.on_press(Message::OpenStartModal);
    }
    row![
        text("Sessions").size(TITLE_SIZE),
        iced::widget::space().width(Fill),
        button(text("Assistant").size(14))
            .padding([8, 10])
            .on_press(Message::OpenAssistantModal)
            .style(iced::widget::button::secondary),
        button(text("Activity").size(14))
            .padding([8, 10])
            .on_press(Message::OpenInbox)
            .style(iced::widget::button::secondary),
        hosts_button(app),
        new_session,
    ]
    .spacing(8)
    .align_y(Center)
    .into()
}

fn config_error_banner(error: &str) -> Element<'_, Message> {
    container(text(format!("configuration error: {error}")).size(ROW_DETAIL_SIZE))
        .padding([8, 12])
        .width(Fill)
        .style(|theme: &Theme| {
            let pair = theme.extended_palette().danger.weak;
            iced::widget::container::Style {
                background: Some(Background::Color(pair.color)),
                text_color: Some(pair.text),
                border: iced::border::rounded(CHIP_RADIUS),
                ..iced::widget::container::Style::default()
            }
        })
        .into()
}

/// Chip labels for `choices`; a label shared by several projects is qualified
/// with its host so every chip stays distinguishable.
fn chip_labels(choices: &[ProjectChoice]) -> Vec<String> {
    let mut occurrences: BTreeMap<&str, usize> = BTreeMap::new();
    for choice in choices {
        *occurrences.entry(choice.label.as_str()).or_default() += 1;
    }
    choices
        .iter()
        .map(|choice| {
            let label = if occurrences[choice.label.as_str()] > 1 {
                format!("{}  ·  {}", choice.label, choice.project.host_id)
            } else {
                choice.label.clone()
            };
            format!("{label}  {}", choice.session_count)
        })
        .collect()
}

fn project_filter_row<'a>(
    choices: &[ProjectChoice],
    total: usize,
    active: Option<&ProjectRef>,
) -> Element<'a, Message> {
    let mut chips = row![filter_chip(
        format!("{ALL_PROJECTS_LABEL}  {total}"),
        None,
        active.is_none()
    )]
    .spacing(6)
    .align_y(Center);
    for (choice, label) in choices.iter().zip(chip_labels(choices)) {
        chips = chips.push(filter_chip(
            label,
            Some(choice.project.clone()),
            active == Some(&choice.project),
        ));
    }
    chips.wrap().into()
}

fn filter_chip<'a>(
    label: String,
    target: Option<ProjectRef>,
    selected: bool,
) -> Element<'a, Message> {
    let chip = button(text(label).size(ROW_META_SIZE))
        .padding([3, 10])
        .on_press(Message::SetProjectFilter(target));
    if selected {
        chip.style(iced::widget::button::primary).into()
    } else {
        chip.style(iced::widget::button::secondary).into()
    }
}

fn session_group(group: SessionGroup, rows: &[SessionRow]) -> Element<'static, Message> {
    let matching: Vec<&SessionRow> = rows.iter().filter(|row| row.group == group).collect();
    let mut list = column![row![
        text(group_label(group)).size(18),
        text(matching.len().to_string()).size(ROW_DETAIL_SIZE),
    ]
    .spacing(8)
    .align_y(Center)]
    .spacing(6);
    for session in matching {
        list = list.push(session_row(session));
    }
    card(list)
}

fn session_row(row: &SessionRow) -> Element<'static, Message> {
    let mut heading = row![text(row.display_name().to_owned()).size(ROW_TITLE_SIZE)]
        .spacing(6)
        .align_y(Center);
    if let Some(attention) = &row.attention {
        let label = match attention.kind {
            NotificationKind::ApprovalRequired => "Approval needed",
            NotificationKind::AgentBlocked => "Input needed",
            NotificationKind::Error => "Review failure",
            _ => "Needs you",
        };
        heading = heading.push(status_pill(label, PillTone::Danger));
    }

    let mut location = row![project_chip(row.project_label.as_deref())]
        .spacing(8)
        .align_y(Center);
    if let Some(branch) = &row.branch {
        location = location.push(
            text(branch.clone())
                .size(ROW_DETAIL_SIZE)
                .font(iced::Font::MONOSPACE),
        );
    }

    let target_host = row.host_id.clone();
    let target_session = row.session_id.clone();
    let info = list_button(
        column![
            heading,
            location,
            text(session_meta(row))
                .size(ROW_META_SIZE)
                .style(muted_style)
        ]
        .spacing(3),
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

/// Muted detail line: host, agent, state, activity, attention title and age.
fn session_meta(row: &SessionRow) -> String {
    let mut meta = row.host_id.to_string();
    push_meta(&mut meta, &row.agent);
    push_meta(&mut meta, row.state.as_str());
    if let Some(activity) = row.activity {
        push_meta(&mut meta, activity_label(activity));
    }
    if let Some(attention) = &row.attention {
        push_meta(&mut meta, &attention.title);
    }
    push_meta(
        &mut meta,
        &format!("updated {}", notification_age_label(&row.updated_at)),
    );
    meta
}

/// Prominent project tag shown on every session row.
fn project_chip(label: Option<&str>) -> Element<'static, Message> {
    let assigned = label.is_some();
    container(text(label.unwrap_or(NO_PROJECT_LABEL).to_owned()).size(ROW_DETAIL_SIZE))
        .padding([2, 8])
        .style(move |theme: &Theme| {
            let palette = theme.extended_palette();
            let pair = if assigned {
                palette.primary.weak
            } else {
                palette.secondary.weak
            };
            iced::widget::container::Style {
                background: Some(Background::Color(pair.color)),
                text_color: Some(pair.text),
                border: iced::border::rounded(CHIP_RADIUS),
                ..iced::widget::container::Style::default()
            }
        })
        .into()
}

fn group_label(group: SessionGroup) -> &'static str {
    match group {
        SessionGroup::NeedsYou => "Needs you",
        SessionGroup::Ready => "Ready",
        SessionGroup::Running => "Running",
        SessionGroup::Unavailable => "Unavailable",
    }
}

fn activity_label(activity: AgentActivity) -> &'static str {
    match activity {
        AgentActivity::Idle => "ready",
        AgentActivity::Working => "working",
        AgentActivity::Blocked => "waiting for input",
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
    use pohunek_gui_core::HostId;

    use super::*;

    fn choice(host: &str, project: &str, label: &str, session_count: usize) -> ProjectChoice {
        ProjectChoice {
            project: ProjectRef {
                host_id: HostId::new(host),
                project_id: project.to_owned(),
            },
            label: label.to_owned(),
            host_connected: true,
            known: true,
            session_count,
        }
    }

    #[test]
    fn group_labels_match_priority_sections() {
        assert_eq!(group_label(SessionGroup::NeedsYou), "Needs you");
        assert_eq!(group_label(SessionGroup::Ready), "Ready");
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

    #[test]
    fn chip_labels_add_the_host_only_to_colliding_labels() {
        let labels = chip_labels(&[
            choice("alpha", "p-1", "api", 2),
            choice("beta", "p-2", "api", 1),
            choice("alpha", "p-3", "web", 4),
        ]);
        assert_eq!(labels, ["api  ·  alpha  2", "api  ·  beta  1", "web  4"]);
    }

    #[test]
    fn overview_renders_with_no_hosts_and_a_config_error() {
        let app = PohunekApp::test_default();
        let _ = detail_view(&app);
    }
}
