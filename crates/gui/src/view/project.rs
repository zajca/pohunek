//! Project detail, worktree list, and per-worktree row rendering.
//!
//! Worktrees are also promoted into the right-pane Worktrees tab
//! (`view::detail`); `project_worktrees` is `pub(crate)` for that reuse.

use iced::widget::{button, column, container, row, text, text_input};
use iced::{Background, Center, Element, Fill, Theme};
use pohunek_gui_core::HostId;
use protocol::{ProjectWorktree, SessionId};

use crate::message::Message;
use crate::selection::{scoped_project, selected_project};
use crate::PohunekApp;

use super::{card, section_title, STATUS_DOT};

/// Project surface: identity, rename, and the New-session entry point.
/// Worktrees and the Linear/GitHub browsers live in their own tabs
/// (`view::detail`) rather than stacked underneath this pane.
pub(crate) fn project_pane(app: &PohunekApp) -> Element<'_, Message> {
    column![
        project_detail(app),
        button("New session")
            .on_press(Message::OpenStartModal)
            .style(iced::widget::button::primary),
    ]
    .spacing(16)
    .into()
}

fn project_detail(app: &PohunekApp) -> Element<'_, Message> {
    let mut detail = column![section_title("Project")].spacing(8);
    if let Some((host_id, project)) = selected_project(app) {
        detail = detail
            .push(text(format!("{} / {}", host_id, project.id)).size(16))
            .push(text(format!("label: {}", project.label)).size(14))
            .push(text(format!("repo: {}", project.repo_root.display())).size(14))
            .push(text(format!("source: {}", project.source.as_str())).size(14));
    } else {
        detail = detail.push(text("No project selected").size(16));
    }
    let detail = detail.push(text("Rename").size(15)).push(
        row![
            text_input("new name", &app.project_edit.rename_to)
                .on_input(Message::ProjectRenameToChanged),
            button("Rename")
                .on_press(Message::RenameProject)
                .style(iced::widget::button::secondary),
        ]
        .spacing(8),
    );
    card(detail)
}

/// Worktree surface for the selected project: a scannable list of every git
/// worktree (live session first, then pohunek-owned, then external) with a
/// status dot, branch, ownership and per-row actions, instead of a flat wall of
/// `path branch=… session=…` lines. Per-worktree removal is intentionally absent
/// — the protocol exposes pruning only via project-level Remove + prune.
///
/// Fills the Worktrees tab body, so the list is unscrolled and uncapped here;
/// the right pane's outer `scrollable` (`view::detail::detail_view`) handles
/// overflow for the whole tab.
pub(crate) fn project_worktrees(app: &PohunekApp) -> Element<'_, Message> {
    let refresh = button("Refresh")
        .on_press(Message::ShowProject)
        .style(iced::widget::button::secondary);
    let header = row![
        section_title("Worktrees"),
        iced::widget::space().width(Fill),
        refresh,
    ]
    .align_y(Center);

    let Some((host_id, project)) = scoped_project(app) else {
        return card(column![header, text("No project selected").size(13)].spacing(10));
    };
    let Some(host) = app.workspace.hosts.get(host_id) else {
        return card(column![header, text("Host is not loaded").size(13)].spacing(10));
    };
    let Some(details) = host.project_details.get(&project.id) else {
        return card(
            column![
                header,
                text("Worktree details not loaded yet — Refresh to list them.").size(13),
            ]
            .spacing(10),
        );
    };

    if details.worktrees.is_empty() {
        return card(column![header, text("No worktrees for this project.").size(13)].spacing(10));
    }

    // Live sessions first, then pohunek-owned, then external; stable by path
    // within each group so the list does not jump around between refreshes.
    let mut worktrees: Vec<&ProjectWorktree> = details.worktrees.iter().collect();
    worktrees.sort_by(|a, b| {
        b.session_id
            .is_some()
            .cmp(&a.session_id.is_some())
            .then_with(|| b.owned.cmp(&a.owned))
            .then_with(|| a.path.cmp(&b.path))
    });
    let total = worktrees.len();
    let owned = worktrees.iter().filter(|worktree| worktree.owned).count();
    let active = worktrees
        .iter()
        .filter(|worktree| worktree.session_id.is_some())
        .count();

    let mut list = column![].spacing(6);
    for worktree in worktrees {
        list = list.push(worktree_row(host_id, host, worktree));
    }

    card(
        column![
            header,
            text(format!(
                "{total} worktrees · {owned} owned · {active} active"
            ))
            .size(13),
            list,
        ]
        .spacing(10),
    )
}

/// One worktree row: status dot, basename + meta subtitle, and right-aligned
/// actions (always Copy path; "Open session" when a live session runs in it,
/// which navigates the detail pane to that session).
fn worktree_row<'a>(
    host_id: &'a HostId,
    host: &'a pohunek_gui_core::HostView,
    worktree: &'a ProjectWorktree,
) -> Element<'a, Message> {
    // The branch is the meaningful identifier — basenames collide (most
    // worktrees are named after the repo, e.g. "connection"), so it leads the
    // row; the absolute path and ownership are the wrapping detail line.
    let branch = worktree.branch.as_deref().unwrap_or("detached");
    let owner = if worktree.owned { "owned" } else { "external" };
    let mut meta = format!("{}  ·  {owner}", worktree.path.display());
    if worktree.locked {
        meta.push_str("  ·  locked");
    }
    // `width(Fill)` lets the info column take the remaining width and wrap the
    // long path, so the actions stay inside the card instead of being pushed off
    // the right edge.
    // Paths and branches have no spaces, so default word wrapping cannot break
    // them; `WordOrGlyph` falls back to glyph wrapping so a long path folds
    // inside the column instead of overflowing the card.
    let info = column![
        text(branch)
            .size(14)
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
        text(meta)
            .size(12)
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
            .style(|theme: &Theme| {
                iced::widget::text::Style {
                    color: Some(theme.extended_palette().background.strong.color),
                }
            }),
    ]
    .spacing(2)
    .width(Fill);

    let mut actions = row![button(text("Copy path").size(12))
        .padding([4, 8])
        .on_press(Message::CopyWorktreePath(worktree.path.clone()))
        .style(iced::widget::button::secondary)]
    .spacing(6);
    // Only offer navigation when the session is actually live on this host, so
    // the target session pane has something to show.
    if let Some(session_id) = worktree
        .session_id
        .as_ref()
        .filter(|session_id| host.sessions.contains_key(session_id.as_str()))
    {
        actions = actions.push(
            button(text("Open").size(12))
                .padding([4, 8])
                .on_press(Message::OpenSession {
                    host_id: host_id.clone(),
                    session_id: SessionId(session_id.clone()),
                })
                .style(iced::widget::button::primary),
        );
    }
    // Remove only a pohunek-owned worktree with no live session: the daemon
    // refuses an external worktree (`worktree_not_owned`) and one a live session
    // uses (`worktree_in_use`), so do not offer the button in those cases.
    if worktree.owned && worktree.session_id.is_none() {
        actions = actions.push(
            button(text("Remove").size(12))
                .padding([4, 8])
                .on_press(Message::RemoveWorktree(worktree.path.clone()))
                .style(iced::widget::button::danger),
        );
    }

    let row = row![
        worktree_dot(worktree.owned, worktree.session_id.is_some()),
        info,
        actions,
    ]
    .spacing(10)
    .align_y(Center);

    container(row)
        .padding([8, 10])
        .width(Fill)
        .style(|theme: &Theme| iced::widget::container::Style {
            background: Some(Background::Color(
                theme.extended_palette().background.weak.color,
            )),
            border: iced::border::rounded(6.0),
            ..iced::widget::container::Style::default()
        })
        .into()
}

/// Filled-circle indicator for a worktree: success (green) when a session is
/// live in it, accent when pohunek owns it but is idle, muted for an external
/// worktree pohunek did not create.
fn worktree_dot(owned: bool, active: bool) -> Element<'static, Message> {
    text(STATUS_DOT)
        .size(13)
        .style(move |theme: &Theme| {
            let palette = theme.extended_palette();
            let color = if active {
                palette.success.base.color
            } else if owned {
                palette.primary.base.color
            } else {
                palette.background.strong.color
            };
            iced::widget::text::Style { color: Some(color) }
        })
        .into()
}
