//! Modal contents for session launch, assistant launch, key help, and toasts.

// Rust guideline compliant 2026-10-01

use iced::widget::{
    button, checkbox, column, container, row, scrollable, text, text_editor, text_input,
};
use iced::{Center, Element};
use pohunek_gui_core::Toast;

use crate::keyboard::{KeyBindingHelp, KeyContext};
use crate::message::{FormField, Message};
use crate::selection::project_host;
use crate::view::session::session_name_input;
use crate::PohunekApp;

use super::{dialog_card, muted_style, scrolling_dialog_card};

/// Tallest an open select option list grows before it scrolls.
const SELECT_OPTIONS_MAX_HEIGHT: f32 = 240.0;

pub(crate) fn start_modal_content(app: &PohunekApp) -> Element<'_, Message> {
    let advanced_label = if app.start.show_advanced {
        "Advanced v"
    } else {
        "Advanced >"
    };
    let prompt_label = if app.start.template.is_some() {
        "Prompt (edit before starting)"
    } else {
        "Prompt / initial input (optional)"
    };
    let mut panel = column![
        form_select(app, "Project", FormField::StartProject),
        row![
            form_select(app, "Agent", FormField::StartAgent),
            form_select(app, "Template", FormField::StartTemplate),
        ]
        .spacing(8)
        .align_y(Center),
        session_name_input(app),
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
                    .on_input(Message::StartBranchChanged),
                text_input("base branch override", &app.start.base_branch)
                    .id(crate::keyboard::start_base_branch_input_id())
                    .on_input(Message::StartBaseBranchChanged),
            ]
            .spacing(8),
        );
    }
    let mut start = button("Start session").style(iced::widget::button::primary);
    if project_host(app, app.start.project.as_ref())
        .is_some_and(|host| host.agent_is_launchable(&app.start.agent))
    {
        start = start.on_press(Message::CreateSession);
    }
    scrolling_dialog_card("Start a session", panel.push(start))
}

pub(crate) fn assistant_modal_content(app: &PohunekApp) -> Element<'_, Message> {
    let advanced_label = if app.assistant.show_advanced {
        "Advanced v"
    } else {
        "Advanced >"
    };
    let mut panel = column![
        form_select(app, "Project", FormField::AssistantProject),
        row![
            form_select(app, "Intent", FormField::AssistantIntent),
            form_select(app, "Agent", FormField::AssistantAgent),
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
                        .on_input(Message::AssistantBranchChanged),
                    text_input("base branch override", &app.assistant.base_branch)
                        .id(crate::keyboard::assistant_base_branch_input_id())
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
    let launchable = project_host(app, app.assistant.project.as_ref()).is_some_and(|host| {
        app.assistant.agent.as_deref().map_or_else(
            || !host.launchable_assistant_agents().is_empty(),
            |agent| host.agent_is_assistant_capable(agent),
        )
    });
    let mut start = button("Start assistant").style(iced::widget::button::primary);
    if launchable {
        start = start.on_press(Message::LaunchAssistant);
    }
    scrolling_dialog_card("Start assistant", panel.push(start))
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
    if crate::keyboard::is_submit_chord(
        &key_press.key,
        key_press.modifiers,
        cfg!(target_os = "macos"),
    ) {
        Some(text_editor::Binding::Custom(submit))
    } else {
        text_editor::Binding::from_key_press(key_press)
    }
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

fn form_select<'a>(
    app: &'a PohunekApp,
    label: &'static str,
    field: FormField,
) -> Element<'a, Message> {
    let selected = crate::keyboard::form_select_label(app, field);
    let is_focused = app.form_focus == field;
    let open = app.form_select.filter(|select| select.field == field);
    let control = button(row![text(selected), text("v")].spacing(8))
        .on_press(Message::ToggleFormSelect(field));
    let control = if is_focused {
        control.style(iced::widget::button::primary)
    } else {
        control.style(iced::widget::button::secondary)
    };
    let mut content = column![text(label).size(14), control].spacing(4);
    if let Some(select) = open {
        let mut options = column![].spacing(4);
        for (index, option) in crate::keyboard::form_select_options(app, field)
            .into_iter()
            .enumerate()
        {
            let option_button =
                button(text(option)).on_press(Message::ChooseFormSelect { field, index });
            options = options.push(if select.cursor == index {
                option_button.style(iced::widget::button::primary)
            } else {
                option_button.style(iced::widget::button::secondary)
            });
        }
        content =
            content.push(container(scrollable(options)).max_height(SELECT_OPTIONS_MAX_HEIGHT));
    }
    content.into()
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
    #[test]
    fn ctrl_enter_submits_multiline_forms() {
        use iced::keyboard::Modifiers;
        let enter = iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter);
        assert!(crate::keyboard::is_submit_chord(
            &enter,
            Modifiers::CTRL,
            false
        ));
        assert!(crate::keyboard::is_submit_chord(
            &enter,
            Modifiers::CTRL,
            true
        ));
        assert!(!crate::keyboard::is_submit_chord(
            &enter,
            Modifiers::empty(),
            true
        ));
        assert!(!crate::keyboard::is_submit_chord(
            &enter,
            Modifiers::CTRL | Modifiers::ALT,
            true
        ));
        assert!(crate::keyboard::is_submit_chord(
            &enter,
            Modifiers::COMMAND,
            true
        ));
        assert!(!crate::keyboard::is_submit_chord(
            &enter,
            Modifiers::LOGO,
            false
        ));
    }
}
