//! Linear/GitHub provider tab bodies, filters, action launcher, and PR pill widgets.
//!
//! `linear_provider_view` and `github_provider_view` are promoted directly
//! into the right-pane Linear/GitHub tabs (`view::detail`); the combined
//! Linear|GitHub toggle that used to gate them behind a `project_pane` card is
//! retired in favor of the top-level tab bar.

use iced::widget::{button, column, container, pick_list, row, text, text_input};
use iced::{Background, Center, Color, Element, Theme};
use pohunek_gui_core::{
    providers, session_link_metadata, GitHubProviderScope, GitHubPullRequestStatusKey, HostId,
    Message as CoreMessage, SessionLinkKind, SessionLinkProvider,
};
use protocol::SessionInfo;

use crate::message::Message;
use crate::view::inbox::date_part;

use super::{list_button, muted_style};

/// Renders the action picker and launch button for a selected provider item.
/// When the project defines no matching action, shows guidance rather than a
/// launch button that would fail.
pub(crate) fn action_launcher(
    actions: Vec<String>,
    selected_action: Option<String>,
    launch: Message,
) -> Element<'static, Message> {
    if actions.is_empty() {
        return text("No matching action defined for this project; add one to launch")
            .size(13)
            .into();
    }
    let selected = selected_action
        .filter(|name| actions.contains(name))
        .or_else(|| actions.first().cloned());
    row![
        text("Action").size(13),
        pick_list(actions, selected, Message::SelectAction),
        button("Launch")
            .on_press(launch)
            .style(iced::widget::button::primary),
    ]
    .spacing(8)
    .align_y(Center)
    .into()
}

/// Renders one selectable button per named filter; the active filter is styled
/// as primary. The picked name (or the first, when none is picked) highlights.
fn filter_buttons(
    filter_names: Vec<String>,
    selected: Option<&str>,
    make_message: impl Fn(String) -> Message,
) -> Element<'static, Message> {
    let active = selected
        .filter(|name| filter_names.iter().any(|candidate| candidate == name))
        .map(ToOwned::to_owned)
        .or_else(|| filter_names.first().cloned());
    let mut row = iced::widget::Row::new().spacing(6);
    for name in filter_names {
        let is_active = active.as_deref() == Some(name.as_str());
        let style = if is_active {
            iced::widget::button::primary
        } else {
            iced::widget::button::secondary
        };
        let message = make_message(name.clone());
        row = row.push(button(text(name).size(13)).on_press(message).style(style));
    }
    row.into()
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "owning host_id keeps the returned Iced element lifetime tied only to host state"
)]
pub(crate) fn linear_provider_view(
    host_id: HostId,
    host: &pohunek_gui_core::HostView,
    filter_names: Vec<String>,
) -> Element<'_, Message> {
    let state = &host.provider.linear;
    let filters = filter_buttons(
        filter_names,
        state.selected_filter.as_deref(),
        Message::SelectLinearFilter,
    );
    let mut view = column![
        filters,
        row![
            text_input("search", &state.search).on_input({
                let host_id = host_id.clone();
                move |value| {
                    Message::Core(CoreMessage::LinearProviderSearchChanged {
                        host_id: host_id.clone(),
                        value,
                    })
                }
            }),
            button("Fetch")
                .on_press(Message::FetchLinearIssues)
                .style(iced::widget::button::secondary),
        ]
        .spacing(8)
    ]
    .spacing(8);
    if !state.issues.is_empty() {
        view = view.push(text("Pick an issue, choose an action, then Launch.").size(12));
    }
    for issue in &state.issues {
        let selected = state.selected_issue_id.as_deref() == Some(issue.prompt_item_id());
        view = view.push(linear_issue_row(issue, selected));
    }
    if let Some(error) = &state.last_error {
        view = view.push(text(error).size(13));
    }
    view.into()
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "owning host_id keeps the returned Iced element lifetime tied only to host state"
)]
pub(crate) fn github_provider_view(
    host_id: HostId,
    current_scope: Option<GitHubProviderScope>,
    host: &pohunek_gui_core::HostView,
    filter_names: Vec<String>,
) -> Element<'_, Message> {
    let state = &host.provider.github;
    // The PR filter (gh search) drives `Fetch PRs`; `search` below is a local
    // text filter applied to the already-fetched rows.
    let filters = filter_buttons(
        filter_names,
        state.selected_filter.as_deref(),
        Message::SelectGitHubFilter,
    );
    let pr_filter_row = row![
        filters,
        button("Fetch PRs")
            .on_press(Message::FetchGitHubPullRequests)
            .style(iced::widget::button::secondary),
    ]
    .spacing(8);
    let mut view = column![
        pr_filter_row,
        row![
            text_input("search", &state.search).on_input({
                let host_id = host_id.clone();
                move |value| {
                    Message::Core(CoreMessage::GitHubProviderSearchChanged {
                        host_id: host_id.clone(),
                        value,
                    })
                }
            }),
            button("Fetch issues")
                .on_press(Message::FetchGitHubIssues)
                .style(iced::widget::button::secondary),
            button("Refresh PR status")
                .on_press(Message::FetchGitHubPullRequestStatus)
                .style(iced::widget::button::secondary),
        ]
        .spacing(8)
    ]
    .spacing(8);
    if state.scope != current_scope {
        if state.scope.is_some() {
            view = view.push(text("Fetch GitHub data for the selected project").size(13));
        }
        if let Some(error) = &state.last_error {
            view = view.push(text(error).size(13));
        }
        return view.into();
    }
    view = view.push(text("Open a pull request to launch a session.").size(12));
    view = view.push(text("Pull requests").size(15));
    for pull_request in filtered_pull_requests(state) {
        let selected = state.selected_pull_request == Some(pull_request.number);
        view = view.push(pull_request_row(pull_request, selected));
    }
    view = view.push(text("Issues").size(15));
    for issue in filtered_github_issues(state) {
        view = view.push(list_button(
            text(format!("#{}  {}", issue.number, issue.title)).size(13),
            Message::OpenGitHubIssue(issue.number),
            false,
        ));
    }
    if let Some(error) = &state.last_error {
        view = view.push(text(error).size(13));
    }
    view.into()
}

pub(crate) fn selected_linear_issue_in_state(
    state: &pohunek_gui_core::LinearProviderState,
) -> Option<&providers::linear::LinearIssue> {
    let selected = state.selected_issue_id.as_ref()?;
    state
        .issues
        .iter()
        .find(|issue| issue.prompt_item_id() == selected)
}

pub(crate) fn selected_pull_request_in_state(
    state: &pohunek_gui_core::GitHubProviderState,
) -> Option<&providers::github::GitHubPullRequest> {
    let selected = state.selected_pull_request?;
    state
        .pull_requests
        .iter()
        .find(|pull_request| pull_request.number == selected)
}

pub(crate) fn selected_github_issue_in_state(
    state: &pohunek_gui_core::GitHubProviderState,
) -> Option<&providers::github::GitHubIssue> {
    let selected = state.selected_issue?;
    state.issues.iter().find(|issue| issue.number == selected)
}

fn filtered_pull_requests(
    state: &pohunek_gui_core::GitHubProviderState,
) -> impl Iterator<Item = &providers::github::GitHubPullRequest> {
    let search = state.search.trim().to_lowercase();
    state.pull_requests.iter().filter(move |pull_request| {
        search.is_empty()
            || pull_request.title.to_lowercase().contains(&search)
            || pull_request.number.to_string().contains(&search)
            || pull_request.head_ref_name.to_lowercase().contains(&search)
    })
}

fn filtered_github_issues(
    state: &pohunek_gui_core::GitHubProviderState,
) -> impl Iterator<Item = &providers::github::GitHubIssue> {
    let search = state.search.trim().to_lowercase();
    state.issues.iter().filter(move |issue| {
        search.is_empty()
            || issue.title.to_lowercase().contains(&search)
            || issue.number.to_string().contains(&search)
    })
}

pub(crate) fn linked_pr_status_label(
    host: &pohunek_gui_core::HostView,
    session: &SessionInfo,
) -> String {
    linked_github_status(host, session)
        .map(|status| format!("  [{status}]"))
        .unwrap_or_default()
}

pub(crate) fn linked_github_status(
    host: &pohunek_gui_core::HostView,
    session: &SessionInfo,
) -> Option<String> {
    let link = session_link_metadata(session)?;
    if link.provider != SessionLinkProvider::GitHub || link.kind != SessionLinkKind::PullRequest {
        return None;
    }
    let scope = session
        .project_id
        .as_ref()
        .and_then(|project_id| host.projects.get(project_id))
        .map(GitHubProviderScope::from_project);
    let status_key = scope.map(|scope| GitHubPullRequestStatusKey::new(scope, link.url.clone()));
    Some(
        status_key
            .as_ref()
            .and_then(|key| host.provider.github.pull_request_statuses.get(key))
            .map_or_else(|| "pr status unknown".to_owned(), format_pr_status),
    )
}

fn format_pr_status(status: &providers::github::PullRequestStatus) -> String {
    let review = review_label(&status.review_decision);
    let summary = providers::github::CheckSummary::from_checks(&status.checks);
    format!(
        "review={review} checks={} pass/{} fail/{} pending",
        summary.passed, summary.failed, summary.pending
    )
}

/// Short human label for a review decision.
fn review_label(decision: &providers::github::ReviewDecision) -> &str {
    match decision {
        providers::github::ReviewDecision::Approved => "approved",
        providers::github::ReviewDecision::ChangesRequested => "changes requested",
        providers::github::ReviewDecision::ReviewRequired => "review required",
        providers::github::ReviewDecision::None => "no review",
        providers::github::ReviewDecision::Unknown(value) => value.as_str(),
    }
}

/// Semantic background tone for a status pill.
#[derive(Debug, Clone, Copy)]
pub(crate) enum PillTone {
    Success,
    Danger,
    Warning,
    Neutral,
}

/// A small rounded status pill backed by a themed semantic color.
pub(crate) fn status_pill(label: impl Into<String>, tone: PillTone) -> Element<'static, Message> {
    let label = label.into();
    container(text(label).size(11))
        .padding([1, 6])
        .style(move |theme: &Theme| {
            let palette = theme.extended_palette();
            let pair = match tone {
                PillTone::Success => palette.success.weak,
                PillTone::Danger => palette.danger.weak,
                PillTone::Warning => palette.warning.weak,
                PillTone::Neutral => palette.secondary.weak,
            };
            iced::widget::container::Style {
                background: Some(Background::Color(pair.color)),
                text_color: Some(pair.text),
                border: iced::border::rounded(4.0),
                ..iced::widget::container::Style::default()
            }
        })
        .into()
}

/// A pill summarizing the pull request review decision.
pub(crate) fn review_pill(
    decision: &providers::github::ReviewDecision,
) -> Element<'static, Message> {
    use providers::github::ReviewDecision;
    let (label, tone) = match decision {
        ReviewDecision::Approved => ("review ok", PillTone::Success),
        ReviewDecision::ChangesRequested => ("changes req", PillTone::Danger),
        ReviewDecision::ReviewRequired => ("review req", PillTone::Warning),
        ReviewDecision::None => ("no review", PillTone::Neutral),
        ReviewDecision::Unknown(value) => (value.as_str(), PillTone::Neutral),
    };
    status_pill(label.to_owned(), tone)
}

/// A pill summarizing CI checks as `pass/fail/pending` counts.
pub(crate) fn ci_pill(checks: &[providers::github::CheckRun]) -> Element<'static, Message> {
    use providers::github::CiState;
    let summary = providers::github::CheckSummary::from_checks(checks);
    if summary.total() == 0 {
        return status_pill("no CI", PillTone::Neutral);
    }
    let tone = match summary.state() {
        CiState::Passing => PillTone::Success,
        CiState::Failing => PillTone::Danger,
        CiState::Pending => PillTone::Warning,
        CiState::None => PillTone::Neutral,
    };
    status_pill(
        format!(
            "CI {}/{}/{}",
            summary.passed, summary.failed, summary.pending
        ),
        tone,
    )
}

/// A label pill colored with GitHub's hex color when one is available.
pub(crate) fn label_pill(label: &providers::github::GitHubLabel) -> Element<'static, Message> {
    let name = label.name.clone();
    match color_from_hex(&label.color) {
        Some(background) => {
            let foreground = contrast_text_color(background);
            container(text(name).size(11))
                .padding([1, 6])
                .style(move |_theme: &Theme| iced::widget::container::Style {
                    background: Some(Background::Color(background)),
                    text_color: Some(foreground),
                    border: iced::border::rounded(4.0),
                    ..iced::widget::container::Style::default()
                })
                .into()
        }
        None => status_pill(name, PillTone::Neutral),
    }
}

/// Parses a 6-digit hex color (optional leading `#`) into an opaque color.
fn color_from_hex(hex: &str) -> Option<Color> {
    let hex = hex.strip_prefix('#').unwrap_or(hex);
    if hex.len() != 6 {
        return None;
    }
    let red = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let green = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let blue = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::from_rgb8(red, green, blue))
}

/// Chooses black or white text for legibility on `background`.
fn contrast_text_color(background: Color) -> Color {
    // Perceived luminance (Rec. 601 weights), the same heuristic GitHub uses to
    // pick black-on-light vs white-on-dark label text.
    let luminance = 0.299 * background.r + 0.587 * background.g + 0.114 * background.b;
    // 0.6 keeps mid-tone labels (such as GitHub's yellow) on black text.
    if luminance > 0.6 {
        Color::BLACK
    } else {
        Color::WHITE
    }
}

/// A two-line Linear issue row: identifier (monospace) and title on the first
/// line, a muted branch line below so an operator can scan the list without
/// opening the item modal.
fn linear_issue_row(
    issue: &providers::linear::LinearIssue,
    selected: bool,
) -> Element<'_, Message> {
    let identifier = text(issue.identifier.as_str())
        .size(13)
        .font(iced::Font::MONOSPACE);
    let title_line = row![identifier, text(issue.title.as_str()).size(13)]
        .spacing(8)
        .align_y(Center);
    let branch_line = text(issue.branch.as_str()).size(11).style(muted_style);
    list_button(
        column![title_line, branch_line].spacing(4),
        Message::OpenLinearIssue(issue.prompt_item_id().to_owned()),
        selected,
    )
}

/// A two-line pull request row: a title line and a metadata line.
///
/// The draft badge leads the title so it stays visible when the title wraps.
/// On the metadata line, fixed-size chips (review, CI, labels) come first and
/// the free-text fields (author, branch, diff, date) trail — so if the narrow
/// panel clips, it clips the least-critical text rather than the status chips.
fn pull_request_row(
    pull_request: &providers::github::GitHubPullRequest,
    selected: bool,
) -> Element<'_, Message> {
    let number = text(format!("#{}", pull_request.number))
        .size(13)
        .style(muted_style);
    let title = text(pull_request.title.as_str()).size(13);
    let title_line = if pull_request.is_draft {
        row![status_pill("draft", PillTone::Neutral), number, title]
    } else {
        row![number, title]
    }
    .spacing(8)
    .align_y(Center);

    let mut meta_line = row![
        review_pill(&pull_request.review_decision),
        ci_pill(&pull_request.checks),
    ]
    .spacing(6)
    .align_y(Center);
    for label in &pull_request.labels {
        meta_line = meta_line.push(label_pill(label));
    }

    let mut parts: Vec<String> = Vec::new();
    if let Some(author) = &pull_request.author {
        parts.push(format!("@{author}"));
    }
    parts.push(pull_request.head_ref_name.clone());
    if pull_request.additions > 0 || pull_request.deletions > 0 {
        parts.push(format!(
            "+{}/-{}",
            pull_request.additions, pull_request.deletions
        ));
    }
    if let Some(updated) = &pull_request.updated_at {
        parts.push(date_part(updated).to_owned());
    }
    if !parts.is_empty() {
        meta_line = meta_line.push(text(parts.join("  ·  ")).size(11).style(muted_style));
    }

    list_button(
        column![title_line, meta_line].spacing(4),
        Message::OpenGitHubPullRequest(pull_request.number),
        selected,
    )
}
