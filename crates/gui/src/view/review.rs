//! Review tab: file list, unified diff pane, inline comment editor, and the
//! review tray (`docs/design/track-d-ui-brief.md` §3.9, Track D.6).
//!
//! All state transitions (which file/line is selected, the open comment
//! editor, the active draft review) live in `gui-core`'s `Workspace`/`HostView`
//! (see `pohunek_gui_core::ReviewTabState`); this module only renders that
//! state and dispatches `Message`s, exactly like the other tab bodies.

use iced::widget::{button, column, container, row, scrollable, text, text_input, tooltip};
use iced::{Background, Center, Element, Fill, Theme};
use pohunek_gui_core::{
    DiffFileStatus, DiffLine, DiffLineKind, DiffModel, HostView, Review, ReviewCommentEditor,
    ReviewDiffStatus, ReviewLineTarget, ReviewSide, ReviewSource, ReviewStatus,
};

use crate::message::Message;
use crate::selection::tab_project_scope;
use crate::view::detail::project_scope_placeholder;
use crate::PohunekApp;

use super::{card, list_button, muted_style, section_title};

/// Fixed width of the Review tab's left file-list column.
const FILE_LIST_WIDTH: u32 = 260;
/// Fixed height of the scrollable unified-diff pane.
const DIFF_PANE_HEIGHT: u32 = 420;
/// Left padding used to indent an inline comment/editor row under its line's
/// gutter, so it visually nests under the line it is anchored to.
const COMMENT_INDENT: u16 = 40;

/// Review tab body: routes by the current host's diff status.
pub(crate) fn review_tab_body(app: &PohunekApp) -> Element<'_, Message> {
    let Some((host_id, _)) = tab_project_scope(app) else {
        return project_scope_placeholder();
    };
    let Some(host) = app.workspace.hosts.get(&host_id) else {
        return card(text("Host is not loaded").size(13));
    };
    match &host.review.diff {
        ReviewDiffStatus::Idle => review_idle_placeholder(),
        ReviewDiffStatus::Fetching => card(text("Fetching diff…").size(13)),
        ReviewDiffStatus::Error(message) => card(
            column![
                text("Failed to load diff").size(15),
                text(message.clone()).size(13),
            ]
            .spacing(6),
        ),
        ReviewDiffStatus::Empty { base } => card(text(format!("No changes vs {base}")).size(13)),
        ReviewDiffStatus::Loaded {
            model,
            base,
            truncated,
        } => review_loaded_body(host, model, base, *truncated),
    }
}

/// Shown before any review has been opened for this host: Review is a
/// persistent tab like Linear/GitHub/Worktrees (reachable whenever a project
/// is scoped), but has nothing to show until the operator opens one from a
/// session's worktree or a GitHub pull request row.
fn review_idle_placeholder() -> Element<'static, Message> {
    card(
        column![
            text("No review open").size(15),
            text(
                "Open a review from a session's worktree (Detail tab → \"Review changes\") or \
                 a GitHub pull request (GitHub tab → \"Review diff\")."
            )
            .size(13),
        ]
        .spacing(6),
    )
}

fn review_loaded_body<'a>(
    host: &'a HostView,
    model: &'a DiffModel,
    base: &str,
    truncated: bool,
) -> Element<'a, Message> {
    let mut layout = column![].spacing(12);
    if truncated {
        layout = layout.push(truncated_banner(base));
    }
    layout = layout.push(
        row![
            container(review_file_list(host, model)).width(FILE_LIST_WIDTH),
            container(review_diff_pane(host, model)).width(Fill),
        ]
        .spacing(16),
    );
    layout = layout.push(review_tray(host));
    card(layout)
}

fn truncated_banner(base: &str) -> Element<'static, Message> {
    container(
        text(format!(
            "Diff truncated at the size cap; later files in the change set vs {base} are not shown."
        ))
        .size(12),
    )
    .padding(8)
    .style(iced::widget::container::rounded_box)
    .into()
}

/// Left pane: one row per file, a status glyph, and a comment-count badge.
fn review_file_list<'a>(host: &'a HostView, model: &'a DiffModel) -> Element<'a, Message> {
    let mut list = column![section_title("Files")].spacing(4);
    for (index, file) in model.files.iter().enumerate() {
        let selected = host.review.selected_file == Some(index);
        let count = comment_count_for_file(host.review.active_review.as_ref(), &file.path);
        let mut label = row![
            text(file_status_glyph(&file.status))
                .size(13)
                .font(iced::Font::MONOSPACE),
            text(file.path.clone()).size(13),
        ]
        .spacing(8)
        .align_y(Center);
        if count > 0 {
            label = label.push(text(format!("({count})")).size(11).style(muted_style));
        }
        list = list.push(list_button(
            label,
            Message::SelectReviewFile(index),
            selected,
        ));
    }
    list.into()
}

/// Right pane: the selected file's hunks, rendered as a scrollable unified
/// diff with inline comments and the open comment editor.
fn review_diff_pane<'a>(host: &'a HostView, model: &'a DiffModel) -> Element<'a, Message> {
    let Some(file_index) = host.review.selected_file else {
        return text("Select a file to view its diff.").size(13).into();
    };
    let Some(file) = model.files.get(file_index) else {
        return text("Select a file to view its diff.").size(13).into();
    };
    if file.hunks.is_empty() {
        return text(no_hunks_message(&file.status)).size(13).into();
    }
    let review = host.review.active_review.as_ref();
    let mut pane = column![].spacing(2);
    for (hunk_index, hunk) in file.hunks.iter().enumerate() {
        pane = pane.push(
            text(hunk.header.clone())
                .size(12)
                .font(iced::Font::MONOSPACE)
                .style(muted_style),
        );
        for (line_index, line) in hunk.lines.iter().enumerate() {
            let target = ReviewLineTarget {
                file_index,
                hunk_index,
                line_index,
            };
            let selected = host.review.selected_line == Some(target);
            pane = pane.push(review_line_row(target, line, selected));
            if let Some((side, number)) = line_anchor(line) {
                if let Some(review) = review {
                    for (comment_index, comment) in
                        review_comments_at(review, &file.path, side, number)
                    {
                        pane = pane.push(review_comment_row(comment_index, &comment.text));
                    }
                }
            }
            if selected {
                pane = pane.push(review_line_action(host.review.comment_editor.as_ref()));
            }
        }
    }
    scrollable(pane).height(DIFF_PANE_HEIGHT).into()
}

/// One diff line: old/new gutters, the +/-/context marker, and the line text.
fn review_line_row(
    target: ReviewLineTarget,
    line: &DiffLine,
    selected: bool,
) -> Element<'_, Message> {
    let marker = match line.kind {
        DiffLineKind::Add => "+",
        DiffLineKind::Remove => "-",
        DiffLineKind::Context => " ",
    };
    let old = line
        .old_line
        .map_or_else(String::new, |number| number.to_string());
    let new = line
        .new_line
        .map_or_else(String::new, |number| number.to_string());
    let content = row![
        text(format!("{old:>5}"))
            .size(12)
            .font(iced::Font::MONOSPACE)
            .style(muted_style),
        text(format!("{new:>5}"))
            .size(12)
            .font(iced::Font::MONOSPACE)
            .style(muted_style),
        text(format!("{marker} {}", line.text))
            .size(12)
            .font(iced::Font::MONOSPACE),
    ]
    .spacing(8);
    button(content)
        .width(Fill)
        .padding([2, 8])
        .on_press(Message::SelectReviewLine(target))
        .style(review_line_style(line.kind, selected))
        .into()
}

/// Button style for one diff line: a translucent tint by add/remove/context
/// kind, with a primary-colored border when selected (mirrors
/// `super::list_row_style`'s selection treatment, tinted per line kind
/// instead of a flat selection background since the kind tint must stay
/// visible even while selected).
fn review_line_style(
    kind: DiffLineKind,
    selected: bool,
) -> impl Fn(&Theme, iced::widget::button::Status) -> iced::widget::button::Style {
    move |theme, status| {
        use iced::widget::button::{Status, Style};
        let palette = theme.extended_palette();
        let mut style = Style {
            background: Some(Background::Color(match kind {
                DiffLineKind::Add => palette.success.weak.color,
                DiffLineKind::Remove => palette.danger.weak.color,
                DiffLineKind::Context => palette.background.base.color,
            })),
            text_color: palette.background.base.text,
            border: iced::border::rounded(4.0),
            ..Style::default()
        };
        if selected {
            style.border = style.border.width(2.0).color(palette.primary.base.color);
        } else if matches!(status, Status::Hovered | Status::Pressed) {
            style.background = Some(Background::Color(palette.background.weak.color));
        }
        style
    }
}

/// Left-indents a row under its line's gutter, with a small top/bottom
/// margin, so an inline comment/editor visually nests under the line it is
/// anchored to.
fn comment_indent_padding() -> iced::Padding {
    iced::Padding::default()
        .top(2.0)
        .bottom(2.0)
        .left(f32::from(COMMENT_INDENT))
}

/// Under the selected line: the open comment editor when it targets this
/// line, otherwise a small "+ Comment" affordance to open one.
fn review_line_action(editor: Option<&ReviewCommentEditor>) -> Element<'_, Message> {
    match editor {
        Some(editor) => review_comment_editor_view(editor),
        None => container(
            button(text("+ Comment").size(11))
                .on_press(Message::BeginReviewComment)
                .style(iced::widget::button::text),
        )
        .padding(iced::Padding::default().left(f32::from(COMMENT_INDENT)))
        .into(),
    }
}

/// The open inline comment editor: a text input plus Save/Cancel.
fn review_comment_editor_view(editor: &ReviewCommentEditor) -> Element<'_, Message> {
    container(
        column![
            // `on_submit` matters beyond convenience: per `keyboard.rs`'s
            // module docs, a focused `text_input` only captures `Enter` when
            // `on_submit` is set. Without it, `Enter` would leak past this
            // field to the global router and re-trigger `BeginReviewComment`
            // (the Enter shortcut that opens this very editor), discarding
            // whatever the operator had just typed.
            text_input("Comment", &editor.draft_text)
                .on_input(Message::ReviewCommentDraftChanged)
                .on_submit(Message::SaveReviewComment),
            row![
                button("Save")
                    .on_press(Message::SaveReviewComment)
                    .style(iced::widget::button::primary),
                button("Cancel")
                    .on_press(Message::CancelReviewComment)
                    .style(iced::widget::button::secondary),
            ]
            .spacing(8),
        ]
        .spacing(6),
    )
    .padding(comment_indent_padding())
    .into()
}

/// One existing comment anchored under its line, with Edit/Delete actions.
fn review_comment_row(index: usize, text_value: &str) -> Element<'_, Message> {
    container(
        row![
            text(text_value).size(12),
            button(text("Edit").size(11))
                .on_press(Message::BeginEditReviewComment(index))
                .style(iced::widget::button::secondary),
            button(text("Delete").size(11))
                .on_press(Message::RemoveReviewComment(index))
                .style(iced::widget::button::danger),
        ]
        .spacing(8)
        .align_y(Center),
    )
    .padding(comment_indent_padding())
    .into()
}

/// The review tray: a comment-count header, every collected comment across
/// files, and the dispatch action.
fn review_tray(host: &HostView) -> Element<'_, Message> {
    let Some(review) = &host.review.active_review else {
        return row![].into();
    };
    let count = review.comments.len();
    let plural = if count == 1 { "" } else { "s" };
    let mut tray = column![row![
        text(format!("Review ({count} comment{plural})")).size(18),
        iced::widget::space().width(Fill),
        dispatch_action(review),
    ]
    .align_y(Center),]
    .spacing(8);
    for (index, comment) in review.comments.iter().enumerate() {
        tray = tray.push(
            row![
                text(format!(
                    "{}:{} ({})",
                    comment.path,
                    comment.line,
                    comment.side.as_str()
                ))
                .size(11)
                .font(iced::Font::MONOSPACE)
                .style(muted_style),
                text(comment.text.clone()).size(12),
                button(text("Edit").size(11))
                    .on_press(Message::BeginEditReviewComment(index))
                    .style(iced::widget::button::secondary),
                button(text("Delete").size(11))
                    .on_press(Message::RemoveReviewComment(index))
                    .style(iced::widget::button::danger),
            ]
            .spacing(8)
            .align_y(Center),
        );
    }
    card(tray)
}

/// The tray's "Dispatch as session…" action: only reachable for a
/// session-sourced draft (`dispatch_review`, `crate::review::dispatch`,
/// requires an existing worktree-bound session to reuse — there is no such
/// session to fall back to for a pull-request-sourced review), disabled with
/// an explanatory tooltip otherwise; a dispatched review shows its target
/// session instead of a button.
fn dispatch_action(review: &Review) -> Element<'_, Message> {
    if review.status == ReviewStatus::Dispatched {
        let session_id = review
            .dispatched_session_id
            .as_ref()
            .map_or("?", |id| id.0.as_str());
        return text(format!("Dispatched as session {session_id}"))
            .size(12)
            .into();
    }
    match &review.source {
        ReviewSource::Session { .. } => button("Dispatch as session…")
            .on_press(Message::OpenReviewDispatchModal)
            .style(iced::widget::button::primary)
            .into(),
        ReviewSource::PullRequest { .. } => tooltip(
            button("Dispatch as session…").style(iced::widget::button::secondary),
            container(
                text("Open this pull request from an existing session's worktree to dispatch a review")
                    .size(12),
            )
            .padding(6)
            .style(iced::widget::container::rounded_box),
            tooltip::Position::Bottom,
        )
        .into(),
    }
}

fn comment_count_for_file(review: Option<&Review>, path: &str) -> usize {
    review.map_or(0, |review| {
        review
            .comments
            .iter()
            .filter(|comment| comment.path == path)
            .count()
    })
}

fn review_comments_at<'a>(
    review: &'a Review,
    path: &str,
    side: ReviewSide,
    line: u32,
) -> Vec<(usize, &'a pohunek_gui_core::ReviewComment)> {
    review
        .comments
        .iter()
        .enumerate()
        .filter(|(_, comment)| comment.path == path && comment.side == side && comment.line == line)
        .collect()
}

/// Resolves the `side`/`line` a new comment on `line` would anchor to: the
/// new-side line number when present (an added or context line), otherwise
/// the old-side number (a removed line has no new-side counterpart). Purely
/// a display-time derivation for matching existing comments to their line;
/// the canonical version used to persist a new comment lives in `gui-core`
/// (`Workspace::begin_review_comment`).
fn line_anchor(line: &DiffLine) -> Option<(ReviewSide, u32)> {
    match (line.new_line, line.old_line) {
        (Some(new_line), _) => Some((ReviewSide::New, new_line)),
        (None, Some(old_line)) => Some((ReviewSide::Old, old_line)),
        (None, None) => None,
    }
}

fn file_status_glyph(status: &DiffFileStatus) -> &'static str {
    match status {
        DiffFileStatus::Modified => "M",
        DiffFileStatus::Added => "A",
        DiffFileStatus::Deleted => "D",
        DiffFileStatus::Renamed { .. } => "R",
        DiffFileStatus::Binary => "B",
    }
}

fn no_hunks_message(status: &DiffFileStatus) -> &'static str {
    match status {
        DiffFileStatus::Binary => "Binary file; no textual diff available.",
        DiffFileStatus::Renamed { .. } => "File renamed with no content changes.",
        DiffFileStatus::Modified | DiffFileStatus::Added | DiffFileStatus::Deleted => {
            "No textual changes (mode-only change)."
        }
    }
}
