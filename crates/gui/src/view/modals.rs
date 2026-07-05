//! Modal dialog contents: start session, start assistant, provider item, and toasts.

use iced::widget::{
    button, checkbox, column, container, pick_list, row, scrollable, text, text_editor, text_input,
};
use iced::{Center, Element};
use pohunek_gui_core::assistant::Intent as AssistantIntent;
use pohunek_gui_core::{ProviderPanel, Toast};
use protocol::ProviderKind;

use crate::message::{AgentChoice, Message, ASSISTANT_AUTO_AGENT_LABEL, BLANK_TEMPLATE_LABEL};
use crate::selection::{available_actions, selected_assistant_project, selected_host_id};
use crate::view::provider::{
    action_launcher, selected_github_issue_in_state, selected_linear_issue_in_state,
    selected_pull_request_in_state,
};
use crate::view::session::session_name_input;
use crate::PohunekApp;

use super::dialog_card;

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
        ProviderPanel::Linear => {
            let Some(issue) = selected_linear_issue_in_state(&host.provider.linear) else {
                return dialog_card("Linear issue", text("No issue selected").size(13));
            };
            let body = column![
                text(format!("{}  {}", issue.prompt_item_id(), issue.title)).size(16),
                text(issue.url.clone()).size(13),
                scrollable(text(issue.body.clone()).size(13)).height(260),
                session_name_input(app),
                action_launcher(
                    available_actions(app, &ProviderKind::LinearIssue),
                    selected_action,
                    Message::LaunchLinearIssue,
                ),
            ]
            .spacing(10);
            dialog_card("Linear issue", body)
        }
        ProviderPanel::GitHub => {
            if let Some(pull_request) = selected_pull_request_in_state(&host.provider.github) {
                let body = column![
                    text(format!("#{}  {}", pull_request.number, pull_request.title)).size(16),
                    text(format!(
                        "{}  {}",
                        pull_request.head_ref_name, pull_request.url
                    ))
                    .size(13),
                    scrollable(text(pull_request.body.clone()).size(13)).height(260),
                    session_name_input(app),
                    action_launcher(
                        available_actions(app, &ProviderKind::GithubPr),
                        selected_action,
                        Message::LaunchGitHubPullRequest,
                    ),
                ]
                .spacing(10);
                return dialog_card("Pull request", body);
            }
            if let Some(issue) = selected_github_issue_in_state(&host.provider.github) {
                let body = column![
                    text(format!("#{}  {}", issue.number, issue.title)).size(16),
                    text(issue.url.clone()).size(13),
                    scrollable(text(issue.body.clone()).size(13)).height(260),
                    text("GitHub issues are reference-only; launch from a pull request.").size(12),
                ]
                .spacing(10);
                return dialog_card("GitHub issue", body);
            }
            dialog_card("GitHub", text("No item selected").size(13))
        }
    }
}

pub(crate) fn toast_view(toast: &Toast) -> Element<'_, Message> {
    container(text(format!(
        "{} / {}: {}",
        toast.host_id, toast.session_id.0, toast.message
    )))
    .padding(8)
    .into()
}
