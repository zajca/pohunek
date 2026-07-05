//! Modal dialog contents: start session, start assistant, provider item, and toasts.

use iced::widget::{
    button, checkbox, column, container, pick_list, row, scrollable, text, text_editor, text_input,
};
use iced::{Center, Element};
use pohunek_gui_core::assistant::Intent as AssistantIntent;
use pohunek_gui_core::{providers, ProviderPanel, Toast};
use protocol::ProviderKind;

use crate::message::{AgentChoice, Message, ASSISTANT_AUTO_AGENT_LABEL, BLANK_TEMPLATE_LABEL};
use crate::selection::{available_actions, selected_assistant_project, selected_host_id};
use crate::view::inbox::notification_age_label;
use crate::view::provider::{
    action_launcher, ci_pill, label_pill, review_pill, selected_github_issue_in_state,
    selected_linear_issue_in_state, selected_pull_request_in_state, status_pill, PillTone,
};
use crate::view::session::session_name_input;
use crate::PohunekApp;

use super::{dialog_card, muted_style};

/// "Start a session" modal. The operator picks the agent and an optional
/// template; the prompt editor holds the session input (typed for a blank
/// session, or the editable rendered template). Branch/base overrides for a
/// blank session hide behind Advanced; a template supplies its own.
pub(crate) fn start_modal_content(app: &PohunekApp) -> Element<'_, Message> {
    let advanced_label = if app.start.show_advanced {
        "Advanced v"
    } else {
        "Advanced >"
    };
    let mut template_options = vec![BLANK_TEMPLATE_LABEL.to_owned()];
    template_options.extend(available_actions(app, &ProviderKind::None));
    let template_selected = Some(
        app.start
            .template
            .clone()
            .unwrap_or_else(|| BLANK_TEMPLATE_LABEL.to_owned()),
    );
    let prompt_label = if app.start.template.is_some() {
        "Prompt (edit before starting)"
    } else {
        "Prompt / initial input (optional)"
    };
    let mut panel = column![
        row![
            text("Agent").size(14),
            pick_list(
                AgentChoice::ALL,
                Some(app.start.agent),
                Message::StartAgentSelected
            ),
            text("Template").size(14),
            pick_list(
                template_options,
                template_selected,
                Message::StartTemplateSelected
            ),
        ]
        .spacing(8)
        .align_y(Center),
        session_name_input(app),
        text(prompt_label).size(13),
        text_editor(&app.prompt_editor)
            .height(220)
            .on_action(Message::PromptEdited),
        button(text(advanced_label).size(13))
            .on_press(Message::ToggleStartAdvanced)
            .style(iced::widget::button::text),
    ]
    .spacing(8);
    if app.start.show_advanced && app.start.template.is_none() {
        panel = panel.push(
            row![
                text_input("branch override", &app.start.branch)
                    .on_input(Message::StartBranchChanged),
                text_input("base branch override", &app.start.base_branch)
                    .on_input(Message::StartBaseBranchChanged),
            ]
            .spacing(8),
        );
    }
    let panel = panel.push(
        button("Start session")
            .on_press(Message::CreateSession)
            .style(iced::widget::button::primary),
    );
    dialog_card("Start a session", panel)
}

pub(crate) fn assistant_modal_content(app: &PohunekApp) -> Element<'_, Message> {
    let advanced_label = if app.assistant.show_advanced {
        "Advanced v"
    } else {
        "Advanced >"
    };
    let context = selected_assistant_project(app).map_or_else(std::convert::identity, |target| {
        format!("{}  ·  {}", target.host.id, target.project_ref)
    });
    let agent_options = assistant_agent_options(app);
    let selected_agent = Some(
        app.assistant
            .agent
            .clone()
            .unwrap_or_else(|| ASSISTANT_AUTO_AGENT_LABEL.to_owned()),
    );
    let mut panel = column![
        text(context).size(13),
        row![
            text("Intent").size(14),
            pick_list(
                [
                    AssistantIntent::Help,
                    AssistantIntent::Setup,
                    AssistantIntent::Project,
                    AssistantIntent::Update,
                    AssistantIntent::Debug,
                ],
                Some(app.assistant.intent),
                Message::AssistantIntentSelected,
            ),
            text("Agent").size(14),
            pick_list(
                agent_options,
                selected_agent,
                Message::AssistantAgentSelected,
            ),
        ]
        .spacing(8)
        .align_y(Center),
        text("Request / initial prompt").size(13),
        text_editor(&app.assistant_editor)
            .height(180)
            .on_action(Message::AssistantRequestEdited),
        button(text(advanced_label).size(13))
            .on_press(Message::ToggleAssistantAdvanced)
            .style(iced::widget::button::text),
    ]
    .spacing(8);
    if app.assistant.show_advanced {
        panel = panel
            .push(
                row![
                    text_input("branch override", &app.assistant.branch)
                        .on_input(Message::AssistantBranchChanged),
                    text_input("base branch override", &app.assistant.base_branch)
                        .on_input(Message::AssistantBaseBranchChanged),
                ]
                .spacing(8),
            )
            .push(
                row![
                    checkbox(app.assistant.no_snapshot)
                        .label("No snapshot")
                        .on_toggle(Message::AssistantNoSnapshotToggled),
                    checkbox(app.assistant.degraded)
                        .label("Degraded")
                        .on_toggle(Message::AssistantDegradedToggled),
                ]
                .spacing(12),
            );
    }
    let panel = panel.push(
        button("Start assistant")
            .on_press(Message::LaunchAssistant)
            .style(iced::widget::button::primary),
    );
    dialog_card("Start assistant", panel)
}

fn assistant_agent_options(app: &PohunekApp) -> Vec<String> {
    let mut options = vec![
        ASSISTANT_AUTO_AGENT_LABEL.to_owned(),
        "pohunek-assistant".to_owned(),
        "codex".to_owned(),
        "claude".to_owned(),
    ];
    if let Ok(host_id) = selected_host_id(app) {
        if let Some(host) = app.workspace.hosts.get(&host_id) {
            for session in host.sessions.values() {
                if session.agent != "shell"
                    && !options.iter().any(|option| option == &session.agent)
                {
                    options.push(session.agent.clone());
                }
            }
        }
    }
    options
}

/// Modal showing the selected provider item's detail and its launch action.
pub(crate) fn provider_item_modal_content(app: &PohunekApp) -> Element<'_, Message> {
    let host_id = match selected_host_id(app) {
        Ok(host_id) => host_id,
        Err(err) => return dialog_card("Provider item", text(err).size(13)),
    };
    let Some(host) = app.workspace.hosts.get(&host_id) else {
        return dialog_card("Provider item", text("Host is not loaded").size(13));
    };
    let selected_action = app.selected_action.clone();
    match host.provider.active_panel {
        ProviderPanel::Linear => linear_issue_modal(app, host, selected_action),
        ProviderPanel::GitHub => github_item_modal(app, host, selected_action),
    }
}

/// Linear issue modal: identifier + state pill, wrapping title, an
/// `assignee · updated <age>` meta line (only the fields Linear reported), a
/// branch row, the scrollable body, and the session-name + launch controls.
fn linear_issue_modal<'a>(
    app: &'a PohunekApp,
    host: &'a pohunek_gui_core::HostView,
    selected_action: Option<String>,
) -> Element<'a, Message> {
    let Some(issue) = selected_linear_issue_in_state(&host.provider.linear) else {
        return dialog_card("Linear issue", text("No issue selected").size(13));
    };
    let mut header = row![text(issue.identifier.as_str())
        .size(14)
        .font(iced::Font::MONOSPACE)]
    .spacing(8)
    .align_y(Center);
    if let Some(state) = &issue.state {
        header = header.push(status_pill(
            state.clone(),
            linear_state_tone(issue.state_type.as_deref()),
        ));
    }
    let mut body = column![
        header,
        text(issue.title.clone())
            .size(16)
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
    ]
    .spacing(10);
    if let Some(meta) = linear_issue_meta_line(issue) {
        body = body.push(text(meta).size(12).style(muted_style));
    }
    body = body
        .push(branch_row(issue.branch.clone(), issue.url.clone()))
        .push(scrollable(text(issue.body.clone()).size(13)).height(360))
        .push(session_name_input(app))
        .push(action_launcher(
            available_actions(app, &ProviderKind::LinearIssue),
            selected_action,
            Message::LaunchLinearIssue,
        ));
    dialog_card("Linear issue", body)
}

/// Routes to the selected GitHub pull request or issue modal, whichever the
/// GitHub panel currently has selected.
fn github_item_modal<'a>(
    app: &'a PohunekApp,
    host: &'a pohunek_gui_core::HostView,
    selected_action: Option<String>,
) -> Element<'a, Message> {
    if let Some(pull_request) = selected_pull_request_in_state(&host.provider.github) {
        return github_pull_request_modal(app, pull_request, selected_action);
    }
    if let Some(issue) = selected_github_issue_in_state(&host.provider.github) {
        return github_issue_modal(issue);
    }
    dialog_card("GitHub", text("No item selected").size(13))
}

/// GitHub pull request modal: `#num title` (draft badge leading, as in the
/// list row), an `@author · labels` meta line, a branch row, a
/// review-decision + checks-summary row, the scrollable body, and the
/// session-name + launch controls.
fn github_pull_request_modal<'a>(
    app: &'a PohunekApp,
    pull_request: &'a providers::github::GitHubPullRequest,
    selected_action: Option<String>,
) -> Element<'a, Message> {
    // Draft leads the title so it stays visible when the title wraps,
    // matching the list row's treatment.
    let mut header = row![].spacing(8).align_y(Center);
    if pull_request.is_draft {
        header = header.push(status_pill("draft", PillTone::Neutral));
    }
    header = header
        .push(text(format!("#{}", pull_request.number)).size(14))
        .push(
            text(pull_request.title.clone())
                .size(16)
                .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
        );
    let mut body = column![header].spacing(10);
    let has_author_or_labels = pull_request.author.is_some() || !pull_request.labels.is_empty();
    if has_author_or_labels {
        let mut meta_line = row![].spacing(6).align_y(Center);
        if let Some(author) = &pull_request.author {
            meta_line = meta_line.push(text(format!("@{author}")).size(12).style(muted_style));
        }
        for label in &pull_request.labels {
            meta_line = meta_line.push(label_pill(label));
        }
        body = body.push(meta_line);
    }
    body = body
        .push(branch_row(
            pull_request.head_ref_name.clone(),
            pull_request.url.clone(),
        ))
        .push(
            row![
                review_pill(&pull_request.review_decision),
                ci_pill(&pull_request.checks),
            ]
            .spacing(6)
            .align_y(Center),
        )
        .push(scrollable(text(pull_request.body.clone()).size(13)).height(260))
        .push(session_name_input(app))
        .push(action_launcher(
            available_actions(app, &ProviderKind::GithubPr),
            selected_action,
            Message::LaunchGitHubPullRequest,
        ));
    dialog_card("Pull request", body)
}

/// GitHub issue modal: header, a branch row when GitHub reported one
/// (otherwise a bare Open-in-browser button), the scrollable body, and the
/// reference-only launch guidance (issues have no native launch flow).
fn github_issue_modal(issue: &providers::github::GitHubIssue) -> Element<'_, Message> {
    let mut body =
        column![text(format!("#{}  {}", issue.number, issue.title)).size(16)].spacing(10);
    body = body.push(match &issue.branch {
        Some(branch) => branch_row(branch.clone(), issue.url.clone()),
        None => open_in_browser_button(issue.url.clone()),
    });
    body = body
        .push(scrollable(text(issue.body.clone()).size(13)).height(260))
        .push(text("GitHub issues are reference-only; launch from a pull request.").size(12));
    dialog_card("GitHub issue", body)
}

/// `assignee · updated <age>` meta line, omitting either half when Linear did
/// not report it; `None` when neither field is present.
fn linear_issue_meta_line(issue: &providers::linear::LinearIssue) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(assignee) = &issue.assignee {
        parts.push(assignee.clone());
    }
    if let Some(updated_at) = &issue.updated_at {
        parts.push(format!("updated {}", notification_age_label(updated_at)));
    }
    (!parts.is_empty()).then(|| parts.join("  ·  "))
}

/// Semantic tone for a Linear workflow state, from its `state_type` category
/// (Linear's `backlog`/`unstarted`/`started`/`completed`/`canceled`/`triage`).
/// Unknown or missing categories render neutral.
fn linear_state_tone(state_type: Option<&str>) -> PillTone {
    match state_type {
        Some("completed") => PillTone::Success,
        Some("canceled") => PillTone::Danger,
        Some("started") => PillTone::Warning,
        _ => PillTone::Neutral,
    }
}

/// A `branch [Copy] [Open in browser]` row: monospace branch name plus a
/// clipboard-copy button and an argv-spawned OS-browser-open button for `url`
/// (see `Message::OpenUrl`).
fn branch_row(branch: String, url: String) -> Element<'static, Message> {
    row![
        text(branch.clone()).size(13).font(iced::Font::MONOSPACE),
        button(text("Copy").size(12))
            .padding([4, 8])
            .on_press(Message::CopyText(branch))
            .style(iced::widget::button::secondary),
        open_in_browser_button(url),
    ]
    .spacing(8)
    .align_y(Center)
    .into()
}

/// An `[Open in browser]` button dispatching `Message::OpenUrl(url)`.
fn open_in_browser_button(url: String) -> Element<'static, Message> {
    button(text("Open in browser").size(12))
        .padding([4, 8])
        .on_press(Message::OpenUrl(url))
        .style(iced::widget::button::secondary)
        .into()
}

pub(crate) fn toast_view(toast: &Toast) -> Element<'_, Message> {
    container(text(format!(
        "{} / {}: {}",
        toast.host_id, toast.session_id.0, toast.message
    )))
    .padding(8)
    .into()
}
