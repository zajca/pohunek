//! Read-only text with native mouse and keyboard selection.

// Rust guideline compliant 2026-06-26

use std::borrow::Cow;
use std::fmt;

use iced::advanced::layout::{self, Layout};
use iced::advanced::mouse;
use iced::advanced::renderer;
use iced::advanced::text::highlighter;
use iced::advanced::widget::tree::{self, Tree};
use iced::advanced::{Clipboard, Shell, Widget};
use iced::widget::text::{self, Wrapping};
use iced::widget::text_editor::{self, Binding, Content, KeyPress};
use iced::{
    Background, Border, Color, Element, Event, Font, Length, Pixels, Rectangle, Size, Theme,
};

use crate::message::Message;

type StyleFn<'a> = dyn Fn(&Theme) -> text::Style + 'a;

/// Creates read-only text that supports selection and clipboard copy.
pub(crate) fn selectable_text<'a>(content: impl Into<Cow<'a, str>>) -> SelectableText<'a> {
    SelectableText::new(content)
}

/// Read-only text backed by Iced's editor selection behavior.
pub(crate) struct SelectableText<'a> {
    content: Cow<'a, str>,
    size: Option<Pixels>,
    font: Option<Font>,
    wrapping: Wrapping,
    style: Option<Box<StyleFn<'a>>>,
}

impl fmt::Debug for SelectableText<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SelectableText")
            .field("content", &self.content)
            .field("size", &self.size)
            .field("font", &self.font)
            .field("wrapping", &self.wrapping)
            .finish_non_exhaustive()
    }
}

impl<'a> SelectableText<'a> {
    fn new(content: impl Into<Cow<'a, str>>) -> Self {
        Self {
            content: content.into(),
            size: None,
            font: None,
            wrapping: Wrapping::default(),
            style: None,
        }
    }

    /// Sets the text size.
    pub(crate) fn size(mut self, size: impl Into<Pixels>) -> Self {
        self.size = Some(size.into());
        self
    }

    /// Sets the text font.
    pub(crate) fn font(mut self, font: impl Into<Font>) -> Self {
        self.font = Some(font.into());
        self
    }

    /// Sets the wrapping strategy.
    pub(crate) fn wrapping(mut self, wrapping: Wrapping) -> Self {
        self.wrapping = wrapping;
        self
    }

    /// Sets the text appearance.
    pub(crate) fn style(mut self, style: impl Fn(&Theme) -> text::Style + 'a) -> Self {
        self.style = Some(Box::new(style));
        self
    }

    fn editor<'b>(
        &'b self,
        content: &'b Content,
    ) -> text_editor::TextEditor<'b, highlighter::PlainText, text_editor::Action> {
        let style = self.style.as_deref();
        let mut editor = text_editor::TextEditor::new(content)
            .id(crate::keyboard::read_only_text_input_id())
            .padding(0)
            .wrapping(self.wrapping)
            .on_action(std::convert::identity)
            .key_binding(read_only_binding)
            .style(move |theme, _status| {
                let value = style
                    .and_then(|style| style(theme).color)
                    .unwrap_or_else(|| theme.palette().text);
                let selection = theme.extended_palette().primary.weak.color;

                text_editor::Style {
                    background: Background::Color(Color::TRANSPARENT),
                    border: Border::default(),
                    placeholder: value,
                    value,
                    selection,
                }
            });
        if let Some(size) = self.size {
            editor = editor.size(size);
        }
        if let Some(font) = self.font {
            editor = editor.font(font);
        }
        editor
    }
}

#[derive(Debug)]
struct SelectableState {
    source: String,
    content: Content,
}

impl SelectableState {
    fn new(source: &str) -> Self {
        Self {
            source: source.to_owned(),
            content: Content::with_text(source),
        }
    }

    fn sync(&mut self, source: &str) {
        if self.source != source {
            source.clone_into(&mut self.source);
            self.content = Content::with_text(source);
        }
    }
}

impl Widget<Message, Theme, iced::Renderer> for SelectableText<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<SelectableState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(SelectableState::new(&self.content))
    }

    fn children(&self) -> Vec<Tree> {
        let content = Content::with_text(&self.content);
        let editor = self.editor(&content);
        let widget: &dyn Widget<text_editor::Action, Theme, iced::Renderer> = &editor;
        vec![Tree::new(widget)]
    }

    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<SelectableState>();
        state.sync(&self.content);
        let editor = self.editor(&state.content);
        let widget: &dyn Widget<text_editor::Action, Theme, iced::Renderer> = &editor;
        if let Some(child) = tree.children.first_mut() {
            child.diff(widget);
        } else {
            tree.children.push(Tree::new(widget));
        }
        tree.children.truncate(1);
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Shrink)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let state = tree.state.downcast_mut::<SelectableState>();
        state.sync(&self.content);
        self.editor(&state.content)
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        // Let an enclosing scrollable handle the wheel; this widget only needs
        // pointer drag selection and keyboard selection movement.
        if matches!(event, Event::Mouse(mouse::Event::WheelScrolled { .. })) {
            return;
        }

        let state = tree.state.downcast_mut::<SelectableState>();
        state.sync(&self.content);

        let mut actions = Vec::new();
        let mut editor_shell = Shell::new(&mut actions);
        self.editor(&state.content).update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            &mut editor_shell,
            viewport,
        );
        let captured = editor_shell.is_event_captured();
        let redraw = editor_shell.redraw_request();
        drop(editor_shell);

        let mut changed = false;
        for action in actions {
            changed |= perform_read_only_action(&mut state.content, action);
        }
        if captured {
            shell.capture_event();
        }
        shell.request_redraw_at(redraw);
        if changed {
            shell.request_redraw();
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_ref::<SelectableState>();
        self.editor(&state.content).draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        let state = tree.state.downcast_ref::<SelectableState>();
        self.editor(&state.content).mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn iced::advanced::widget::Operation,
    ) {
        let state = tree.state.downcast_mut::<SelectableState>();
        self.editor(&state.content)
            .operate(&mut tree.children[0], layout, renderer, operation);
    }
}

impl<'a> From<SelectableText<'a>> for Element<'a, Message> {
    fn from(text: SelectableText<'a>) -> Self {
        Self::new(text)
    }
}

fn perform_read_only_action(content: &mut Content, action: text_editor::Action) -> bool {
    if action.is_edit() {
        return false;
    }
    content.perform(action);
    true
}

fn read_only_binding(key_press: KeyPress) -> Option<Binding<text_editor::Action>> {
    match Binding::from_key_press(key_press)? {
        binding @ (Binding::Unfocus
        | Binding::Copy
        | Binding::Move(_)
        | Binding::Select(_)
        | Binding::SelectWord
        | Binding::SelectLine
        | Binding::SelectAll) => Some(binding),
        Binding::Cut
        | Binding::Paste
        | Binding::Insert(_)
        | Binding::Enter
        | Binding::Backspace
        | Binding::Delete
        | Binding::Sequence(_)
        | Binding::Custom(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn state_sync_replaces_changed_text() {
        let mut state = SelectableState::new("before");

        state.sync("after");

        assert_eq!(state.source, "after");
        assert_eq!(state.content.text(), "after");
    }

    #[test]
    fn read_only_actions_reject_every_edit() {
        let edits = [
            text_editor::Edit::Insert('x'),
            text_editor::Edit::Paste(Arc::new("x".to_owned())),
            text_editor::Edit::Enter,
            text_editor::Edit::Indent,
            text_editor::Edit::Unindent,
            text_editor::Edit::Backspace,
            text_editor::Edit::Delete,
        ];

        for edit in edits {
            let mut content = Content::with_text("unchanged");
            assert!(!perform_read_only_action(
                &mut content,
                text_editor::Action::Edit(edit)
            ));
            assert_eq!(content.text(), "unchanged");
        }
    }
}
