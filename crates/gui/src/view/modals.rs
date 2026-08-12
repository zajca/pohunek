//! Modal contents for session launch, assistant launch, key help, and toasts.

// Rust guideline compliant 2026-08-12

use iced::widget::{
    button, checkbox, column, container, pick_list, row, text, text_editor, text_input,
};
use iced::{Center, Element};
use pohunek_gui_core::assistant::Intent as AssistantIntent;
use pohunek_gui_core::Toast;
use protocol::ProviderKind;

use crate::keyboard::{KeyBindingHelp, KeyContext};
use crate::message::{Message, ASSISTANT_AUTO_AGENT_LABEL, BLANK_TEMPLATE_LABEL};
use crate::selection::{available_actions, selected_assistant_project, selected_host_id};
use crate::view::session::session_name_input;
use crate::PohunekApp;

use super::{dialog_card, muted_style};

pub(crate) fn start_modal_content(app: &PohunekApp) -> Element<'_, Message> {
    let advanced_label = if app.start.show_advanced {
        "Advanced v"
    } else {
        "Advanced >"
    };
    let mut template_options = vec![BLANK_TEMPLATE_LABEL.to_owned()];
    template_options.extend(available_actions(app, &ProviderKind::None));
    let selected_template = Some(
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
                start_agent_options(app),
                Some(app.start.agent.clone()),
                Message::StartAgentSelected
            ),
            text("Template").size(14),
            pick_list(
                template_options,
                selected_template,
                Message::StartTemplateSelected
            ),
        ]
        .spacing(8)
        .align_y(Center),
        session_name_input(app, Message::CreateSession),
        text(prompt_label).size(13),
        text_editor(&app.prompt_editor)
            .id(crate::keyboard::start_prompt_input_id())
            .height(220)
            .key_binding(start_prompt_binding)
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
                    .id(crate::keyboard::start_branch_input_id())
                    .on_input(Message::StartBranchChanged)
                    .on_submit(Message::CreateSession),
                text_input("base branch override", &app.start.base_branch)
                    .id(crate::keyboard::start_base_branch_input_id())
                    .on_input(Message::StartBaseBranchChanged)
                    .on_submit(Message::CreateSession),
            ]
            .spacing(8),
        );
    }
    let mut start = button("Start session").style(iced::widget::button::primary);
    if selected_host(app).is_some_and(|host| host.agent_is_launchable(&app.start.agent)) {
        start = start.on_press(Message::CreateSession);
    }
    dialog_card("Start a session", panel.push(start))
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
                assistant_agent_options(app),
                selected_agent,
                Message::AssistantAgentSelected,
            ),
        ]
        .spacing(8)
        .align_y(Center),
        text("Request / initial prompt").size(13),
        text_editor(&app.assistant_editor)
            .id(crate::keyboard::assistant_request_input_id())
            .height(180)
            .key_binding(assistant_request_binding)
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
                        .id(crate::keyboard::assistant_branch_input_id())
                        .on_input(Message::AssistantBranchChanged)
                        .on_submit(Message::LaunchAssistant),
                    text_input("base branch override", &app.assistant.base_branch)
                        .id(crate::keyboard::assistant_base_branch_input_id())
                        .on_input(Message::AssistantBaseBranchChanged)
                        .on_submit(Message::LaunchAssistant),
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
    let launchable = selected_host(app).is_some_and(|host| {
        app.assistant.agent.as_deref().map_or_else(
            || !host.launchable_assistant_agents().is_empty(),
            |agent| host.agent_is_assistant_capable(agent),
        )
    });
    let mut start = button("Start assistant").style(iced::widget::button::primary);
    if launchable {
        start = start.on_press(Message::LaunchAssistant);
    }
    dialog_card("Start assistant", panel.push(start))
}

fn start_prompt_binding(key_press: text_editor::KeyPress) -> Option<text_editor::Binding<Message>> {
    multiline_binding(key_press, Message::CreateSession)
}

fn assistant_request_binding(
    key_press: text_editor::KeyPress,
) -> Option<text_editor::Binding<Message>> {
    multiline_binding(key_press, Message::LaunchAssistant)
}

fn multiline_binding(
    key_press: text_editor::KeyPress,
    submit: Message,
) -> Option<text_editor::Binding<Message>> {
    if is_ctrl_enter(&key_press.key, key_press.modifiers) {
        Some(text_editor::Binding::Custom(submit))
    } else {
        text_editor::Binding::from_key_press(key_press)
    }
}

fn is_ctrl_enter(key: &iced::keyboard::Key, modifiers: iced::keyboard::Modifiers) -> bool {
    matches!(
        key.as_ref(),
        iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter)
    ) && modifiers.control()
        && !modifiers.alt()
        && !modifiers.logo()
}

pub(crate) fn keymap_modal_content(app: &PohunekApp) -> Element<'_, Message> {
    let rows = app.keymap.help_rows();
    dialog_card(
        "Keyboard shortcuts",
        column![
            keymap_section("Global", &rows, KeyContext::Global),
            keymap_section("Modal", &rows, KeyContext::Modal),
        ]
        .spacing(14),
    )
}

fn keymap_section(
    title: &'static str,
    rows: &[KeyBindingHelp],
    context: KeyContext,
) -> Element<'static, Message> {
    let mut section = column![text(title).size(15)].spacing(6);
    for binding in rows.iter().filter(|binding| binding.context == context) {
        section = section.push(
            row![
                text(binding.chord.clone())
                    .size(13)
                    .font(iced::Font::MONOSPACE),
                text(binding.name).size(13).style(muted_style),
            ]
            .spacing(14),
        );
    }
    section.into()
}

fn selected_host(app: &PohunekApp) -> Option<&pohunek_gui_core::HostView> {
    selected_host_id(app)
        .ok()
        .and_then(|host_id| app.workspace.hosts.get(&host_id))
}

fn start_agent_options(app: &PohunekApp) -> Vec<String> {
    selected_host(app).map_or_else(Vec::new, pohunek_gui_core::HostView::launchable_agents)
}

fn assistant_agent_options(app: &PohunekApp) -> Vec<String> {
    let mut options = vec![ASSISTANT_AUTO_AGENT_LABEL.to_owned()];
    if let Some(host) = selected_host(app) {
        options.extend(host.launchable_assistant_agents());
    }
    options
}

pub(crate) fn toast_view(toast: &Toast) -> Element<'_, Message> {
    container(text(format!(
        "{} / {}: {}",
        toast.host_id, toast.session_id.0, toast.message
    )))
    .padding(8)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_enter_submits_multiline_forms() {
        let enter = iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter);
        assert!(is_ctrl_enter(&enter, iced::keyboard::Modifiers::CTRL));
        assert!(!is_ctrl_enter(&enter, iced::keyboard::Modifiers::empty()));
        assert!(!is_ctrl_enter(
            &enter,
            iced::keyboard::Modifiers::CTRL | iced::keyboard::Modifiers::ALT
        ));
    }
}
