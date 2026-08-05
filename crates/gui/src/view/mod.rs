//! Top-level Iced view tree: shared widget helpers and the view submodules.

// `pub(crate)` so `crate::view::review` can reuse `project_scope_placeholder`
// for the Review tab's empty state, the same way `provider` below is widened.
pub(crate) mod detail;
pub(crate) mod inbox;
mod modals;
mod project;
// `pub(crate)` so `crate::keyboard` can reuse the "what's selected" lookups
// (`selected_linear_issue_in_state` and friends) for the provider-item
// modal's `Enter` shortcut, the same way `inbox` above is widened for its
// timestamp helpers.
pub(crate) mod provider;
pub(crate) mod review;
mod session;
mod tree;

use iced::widget::{button, center, column, container, mouse_area, opaque, row, stack, text};
use iced::{Background, Center, Color, Element, Fill, Theme};
use pohunek_gui_core::{ConnState, TreeNodeId};
use protocol::{AgentKind, SessionInfo};

use crate::message::{Message, ModalView};
use crate::PohunekApp;

use detail::detail_view;
use inbox::inbox_modal_content;
use modals::{
    assistant_modal_content, dispatch_review_modal_content, keymap_modal_content,
    provider_item_modal_content, start_modal_content,
};
use tree::{
    agents_monitor, assistant_entry_button, conn_color, inbox_entry_button, workspace_tree,
};

/// Returns a provider-neutral label for an agent kind received from the wire.
fn agent_kind_label(kind: &AgentKind) -> String {
    match kind {
        AgentKind::Shell => "shell".to_owned(),
        AgentKind::Codex => "codex".to_owned(),
        AgentKind::Claude => "claude".to_owned(),
        AgentKind::Hermes => "hermes".to_owned(),
        AgentKind::Unknown(value) => format!("Unknown agent ({value})"),
    }
}

/// Returns the launch profile for known agents and a neutral future-agent label.
fn session_agent_label(session: &SessionInfo) -> String {
    if session.agent_base.is_known() {
        session.agent.clone()
    } else {
        agent_kind_label(&session.agent_base)
    }
}

/// Subtle rounded card that groups a detail section so the pane reads as panels
/// rather than a flat stack of text and buttons.
fn card<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(content)
        .padding(16)
        .width(Fill)
        .style(iced::widget::container::rounded_box)
        .into()
}

/// Heading for a detail card.
fn section_title(label: &str) -> Element<'_, Message> {
    text(label).size(18).into()
}

/// Button style for selectable list rows (tree nodes, provider items, monitor
/// rows): flat and transparent, with a hover tint and a filled accent when
/// selected, so lists read as lists rather than a wall of identical buttons.
fn list_row_style(
    selected: bool,
) -> impl Fn(&Theme, iced::widget::button::Status) -> iced::widget::button::Style {
    move |theme, status| {
        use iced::widget::button::{Status, Style};
        let palette = theme.extended_palette();
        let mut style = Style {
            background: None,
            text_color: palette.background.base.text,
            border: iced::border::rounded(6.0),
            ..Style::default()
        };
        if selected {
            style.background = Some(Background::Color(palette.primary.weak.color));
            style.text_color = palette.primary.weak.text;
        } else if matches!(status, Status::Hovered | Status::Pressed) {
            style.background = Some(Background::Color(palette.background.weak.color));
        }
        style
    }
}

/// A full-width selectable list row.
fn list_button<'a>(
    content: impl Into<Element<'a, Message>>,
    message: Message,
    selected: bool,
) -> Element<'a, Message> {
    button(content)
        .width(Fill)
        .padding([6, 10])
        .on_press(message)
        .style(list_row_style(selected))
        .into()
}

/// A flat expand/collapse caret toggle.
fn caret(expanded: bool, node: TreeNodeId) -> Element<'static, Message> {
    button(text(if expanded { "v" } else { ">" }).size(13))
        .padding([2, 6])
        .on_press(Message::ToggleNode(node))
        .style(iced::widget::button::text)
        .into()
}

pub(crate) fn view(app: &PohunekApp) -> Element<'_, Message> {
    let left = column![
        assistant_entry_button(),
        inbox_entry_button(app),
        container(workspace_tree(app))
            .padding(12)
            .height(Fill)
            .style(iced::widget::container::rounded_box),
        container(agents_monitor(app))
            .padding(12)
            .height(u32::from(app.ui_state.agents_pane_height))
            .style(iced::widget::container::rounded_box)
    ]
    .spacing(12);

    let base = container(row![
        container(left).width(u32::from(app.ui_state.left_pane_width)),
        container(detail_view(app)).padding([0, 16]).width(Fill)
    ])
    .padding(16)
    .width(Fill)
    .height(Fill);
    match app.modal {
        ModalView::None => base.into(),
        ModalView::Start => modal(base.into(), start_modal_content(app), Message::CloseModal),
        ModalView::Assistant => modal(
            base.into(),
            assistant_modal_content(app),
            Message::CloseModal,
        ),
        ModalView::Keymap => modal(base.into(), keymap_modal_content(app), Message::CloseModal),
        ModalView::ProviderItem => modal(
            base.into(),
            provider_item_modal_content(app),
            Message::CloseModal,
        ),
        ModalView::Inbox => modal(base.into(), inbox_modal_content(app), Message::CloseModal),
        ModalView::DispatchReview => modal(
            base.into(),
            dispatch_review_modal_content(app),
            Message::CloseModal,
        ),
    }
}

/// Overlays `dialog` centered on a dimmed backdrop above `base`. Clicking the
/// backdrop sends `on_close`; the dialog itself swallows clicks.
fn modal<'a>(
    base: Element<'a, Message>,
    dialog: Element<'a, Message>,
    on_close: Message,
) -> Element<'a, Message> {
    stack![
        base,
        opaque(
            mouse_area(center(opaque(dialog)).style(|theme: &Theme| {
                iced::widget::container::Style {
                    background: Some(Background::Color(Color {
                        a: 0.8,
                        ..theme.palette().background
                    })),
                    ..iced::widget::container::Style::default()
                }
            }))
            .on_press(on_close)
        )
    ]
    .into()
}

/// A fixed-width rounded dialog body with a title and a close button.
fn dialog_card<'a>(
    title: &'a str,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    let header = row![
        text(title).size(20),
        iced::widget::space().width(Fill),
        button("Close")
            .on_press(Message::CloseModal)
            .style(iced::widget::button::secondary),
    ]
    .align_y(Center);
    container(column![header, content.into()].spacing(16))
        .padding(20)
        .width(640)
        .style(iced::widget::container::rounded_box)
        .into()
}

/// Indents a tree row by depth so the host > project > session hierarchy reads
/// visually without spacer hacks.
fn indent<'a>(depth: u16, content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(content)
        .padding(iced::Padding::ZERO.left(f32::from(depth) * 16.0))
        .into()
}

/// Append `value` to a middot-separated metadata line, adding the separator only
/// when `line` already has content (so it never starts with a stray separator).
fn push_meta(line: &mut String, value: &str) {
    if !line.is_empty() {
        line.push_str("  ·  ");
    }
    line.push_str(value);
}

/// Muted text style for secondary row metadata.
fn muted_style(theme: &Theme) -> iced::widget::text::Style {
    // Dim the foreground text (not a background-derived gray, which is nearly
    // invisible on dark themes) so metadata stays clearly legible.
    let mut color = theme.extended_palette().background.base.text;
    color.a = 0.75;
    iced::widget::text::Style { color: Some(color) }
}

/// U+25CF BLACK CIRCLE: a compact filled status dot that renders consistently
/// across desktop fonts.
const STATUS_DOT: &str = "\u{25CF}";

/// A filled-circle indicator colored by host connection state.
fn conn_dot(conn: ConnState) -> Element<'static, Message> {
    text(STATUS_DOT)
        .size(13)
        .style(move |theme: &Theme| iced::widget::text::Style {
            color: Some(conn_color(theme, &conn)),
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_agent_kind_has_neutral_label() {
        assert_eq!(
            agent_kind_label(&AgentKind::Unknown("future-agent".to_owned())),
            "Unknown agent (future-agent)"
        );
    }

    #[test]
    fn hermes_agent_kind_has_a_stable_label() {
        assert_eq!(agent_kind_label(&AgentKind::Hermes), "hermes");
    }
}
