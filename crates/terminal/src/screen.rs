//! Terminal grid scraping backed by a `vt100` parser.
//!
//! [`ScreenTracker`] feeds raw PTY bytes into a `vt100` grid and exposes visible
//! text, column slices, and agent-oriented prompt/rule heuristics used by the
//! daemon's activity detection. It models only the visible screen (no
//! scrollback); dropped PTY output is recovered by [`ScreenTracker::reset`].

// Rust guideline compliant 2026-06-26

use unicode_width::UnicodeWidthChar;

const SCROLLBACK_LINES: usize = 0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenRegion {
    pub lines: Vec<String>,
}

pub struct ScreenTracker {
    parser: vt100::Parser,
    /// VT and UTF-8 state retained across feeds for character observations.
    draw_parser: Option<vte::Parser>,
}

impl std::fmt::Debug for ScreenTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScreenTracker")
            .field("size", &self.parser.screen().size())
            .finish_non_exhaustive()
    }
}

impl ScreenTracker {
    #[must_use]
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, SCROLLBACK_LINES),
            draw_parser: Some(vte::Parser::new()),
        }
    }

    /// Feeds PTY bytes and reports positive-width character prints.
    ///
    /// A second instance of the same VT state machine used by `vt100` observes
    /// print callbacks, including identical redraws that a final text diff
    /// misses. Both parsers retain partial UTF-8 and escape sequences across
    /// feeds; OSC payloads, cursor movement and other controls do not print.
    pub fn feed(&mut self, bytes: &[u8]) -> bool {
        let mut observation = DrawObservation::default();
        if let Some(parser) = &mut self.draw_parser {
            parser.advance(&mut observation, bytes);
        }
        self.parser.process(bytes);
        observation.drew_chars
    }

    /// Feeds bytes with observations before controls and possible wrapping.
    ///
    /// The observer sees pending text changes before controls or wrapping can
    /// erase them, and any remaining changes after all bytes. Formatting-only
    /// output does not scrape the grid.
    /// Positive-width print observations accompany each rendered prefix.
    /// Parser state survives split UTF-8 and escape sequences. Ordinary text
    /// is batched: the observer never runs for each printable character.
    pub fn feed_observed<F>(&mut self, bytes: &[u8], observe: &mut F)
    where
        F: FnMut(&ScreenTracker, ScreenObservation),
    {
        if self.draw_parser.is_none() {
            self.parser.process(bytes);
            return;
        }
        let mut applied = 0;
        let mut printed = false;
        let mut dirty = false;
        let columns = usize::from(self.parser.screen().size().1);
        let mut column = usize::from(self.parser.screen().cursor_position().1);
        for index in 0..bytes.len() {
            let mut event = ObservedFeed::default();
            if let Some(parser) = &mut self.draw_parser {
                parser.advance(&mut event, &bytes[index..=index]);
            }
            let may_wrap = event.width.is_some_and(|width| column + width > columns);
            if event.completed_action || may_wrap {
                // Exclude this action's final byte: the grid must still show
                // the prefix the action is about to change or scroll away.
                self.parser.process(&bytes[applied..index]);
                if dirty && (event.observe_before || may_wrap) {
                    observe(self, ScreenObservation::new(printed));
                    printed = false;
                    dirty = false;
                }
                self.parser.process(&bytes[index..=index]);
                applied = index + 1;
                column = usize::from(self.parser.screen().cursor_position().1);
            } else if let Some(width) = event.width {
                column += width;
            }
            printed |= event.width.is_some();
            dirty |= event.changes_text;
        }
        self.parser.process(&bytes[applied..]);
        // Include a final erase or scroll, but never scrape an unchanged grid
        // merely because a chunk contains cursor moves or SGR controls.
        if dirty {
            observe(self, ScreenObservation::new(printed));
        }
    }

    /// Permanently ends character and ordered screen observations.
    ///
    /// Grid updates continue through `feed` or `feed_observed`; neither reports
    /// redraws or calls the observer after this, even after a reset.
    pub fn finish_observation(&mut self) {
        self.draw_parser = None;
    }

    /// Whether the parsed terminal has bracketed paste enabled.
    #[must_use]
    pub fn bracketed_paste(&self) -> bool {
        self.parser.screen().bracketed_paste()
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
    }

    /// Discards the current grid by recreating the parser at the current size.
    ///
    /// Used to recover after dropped PTY output: a skipped chunk may have severed
    /// an in-flight escape sequence, leaving the `vt100` grid in a state that no
    /// longer reflects the terminal. We drop the stale grid rather than trust it;
    /// the next agent refresh repaints the screen.
    pub fn reset(&mut self) {
        let (rows, cols) = self.parser.screen().size();
        self.parser = vt100::Parser::new(rows, cols, SCROLLBACK_LINES);
        self.draw_parser = self.draw_parser.as_ref().map(|_| vte::Parser::new());
    }

    #[must_use]
    pub(crate) fn visible_lines(&self) -> Vec<String> {
        let (rows, cols) = self.parser.screen().size();

        (0..rows).map(|row| self.trimmed_line(row, cols)).collect()
    }

    #[must_use]
    pub fn bottom_lines(&self, count: usize) -> ScreenRegion {
        let lines = self.visible_lines();
        let start = lines.len().saturating_sub(count);

        ScreenRegion {
            lines: lines[start..].to_vec(),
        }
    }

    #[must_use]
    pub fn bottom_non_empty_lines(&self, count: usize) -> ScreenRegion {
        let mut lines = self
            .visible_lines()
            .into_iter()
            .rev()
            .filter(|line| !line.is_empty())
            .take(count)
            .collect::<Vec<_>>();
        lines.reverse();

        ScreenRegion { lines }
    }

    /// Returns the first non-empty visible lines, counting from the top.
    #[must_use]
    pub fn top_non_empty_lines(&self, count: usize) -> ScreenRegion {
        let lines = self
            .visible_lines()
            .into_iter()
            .filter(|line| !line.is_empty())
            .take(count)
            .collect();

        ScreenRegion { lines }
    }

    /// Returns a raw terminal-column slice with blank cells preserved.
    ///
    /// Wide glyphs are emitted only when the requested range includes both
    /// terminal cells. Clipped wide glyph cells and continuation cells are
    /// represented with spaces so region geometry remains stable.
    #[must_use]
    pub(crate) fn slice_columns(&self, row: u16, start_col: u16, width: u16) -> String {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        if row >= rows || start_col >= cols || width == 0 {
            return String::new();
        }

        let end_col = start_col.saturating_add(width).min(cols);
        let mut text = String::new();
        let mut col = start_col;

        while col < end_col {
            let Some(cell) = screen.cell(row, col) else {
                text.push(' ');
                col += 1;
                continue;
            };

            if cell.is_wide_continuation() {
                text.push(' ');
                col += 1;
                continue;
            }

            if cell.is_wide() && col.saturating_add(1) >= end_col {
                text.push(' ');
                col += 1;
                continue;
            }

            let contents = cell.contents();
            if contents.is_empty() {
                text.push(' ');
            } else {
                text.push_str(contents);
            }

            col += if cell.is_wide() { 2 } else { 1 };
        }

        text
    }

    #[must_use]
    pub fn recent_text(&self) -> String {
        let mut lines = self.visible_lines();
        while lines.last().is_some_and(std::string::String::is_empty) {
            lines.pop();
        }

        lines.join("\n")
    }

    /// Visible text after the last prompt-marker line.
    ///
    /// Mirrors herdr's `after_last_prompt_marker`: returns the lines following
    /// the last Codex-style prompt marker, or the whole visible text when no
    /// marker is present.
    #[must_use]
    pub fn after_last_prompt_marker(&self) -> String {
        let lines = self.visible_lines();
        match lines
            .iter()
            .rposition(|line| Self::is_prompt_marker_line(line))
        {
            Some(index) => lines[index + 1..].join("\n"),
            None => lines.join("\n"),
        }
    }

    /// Visible text inside the prompt box body.
    ///
    /// Mirrors herdr's `prompt_box_body`: the lines between the prompt-box top
    /// border (the second horizontal rule counting from the bottom) and the next
    /// horizontal rule below it. Returns an empty string when no box is present.
    #[must_use]
    pub fn prompt_box_body(&self) -> String {
        let lines = self.visible_lines();
        let Some(top) = Self::prompt_box_top_border_index(&lines) else {
            return String::new();
        };

        let end = lines[top + 1..]
            .iter()
            .position(|line| Self::is_horizontal_rule(line))
            .map_or(lines.len(), |relative| top + 1 + relative);

        lines[top + 1..end].join("\n")
    }

    /// Returns the last non-empty line above the prompt-box top border.
    ///
    /// Returns an empty string when no Codex-style prompt box is present or no
    /// content remains above its top border.
    #[must_use]
    pub fn last_non_empty_above_prompt_box(&self) -> String {
        let lines = self.visible_lines();
        let Some(top_border) = Self::prompt_box_top_border_index(&lines) else {
            return String::new();
        };

        lines[..top_border]
            .iter()
            .rev()
            .find(|line| !line.is_empty())
            .cloned()
            .unwrap_or_default()
    }

    /// Visible text after the last horizontal-rule line.
    ///
    /// Mirrors herdr's `after_last_horizontal_rule`: returns the lines after the
    /// last horizontal rule, or the whole visible text when no rule is present.
    #[must_use]
    pub fn after_last_horizontal_rule(&self) -> String {
        let lines = self.visible_lines();
        match lines
            .iter()
            .rposition(|line| Self::is_horizontal_rule(line))
        {
            Some(index) => lines[index + 1..].join("\n"),
            None => lines.join("\n"),
        }
    }

    fn trimmed_line(&self, row: u16, cols: u16) -> String {
        self.slice_columns(row, 0, cols)
            .trim_end_matches(' ')
            .to_string()
    }

    /// Codex-style prompt marker line: `›` alone or a `› ` prefix.
    ///
    /// Ported verbatim from herdr's `codex_prompt_line`.
    fn is_prompt_marker_line(line: &str) -> bool {
        line == "›" || line.starts_with("› ")
    }

    /// Horizontal-rule line of `─` (U+2500) runs.
    ///
    /// Ported verbatim from herdr's `is_horizontal_rule`: a trimmed line is a
    /// rule when its leading `─` run spans the whole line or has length >= 3.
    fn is_horizontal_rule(line: &str) -> bool {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return false;
        }

        let rule_chars = trimmed.chars().take_while(|&ch| ch == '─').count();
        if rule_chars == 0 {
            return false;
        }

        let rule_bytes = trimmed
            .char_indices()
            .nth(rule_chars)
            .map_or(trimmed.len(), |(index, _)| index);
        let suffix = trimmed[rule_bytes..].trim_start();

        suffix.is_empty() || rule_chars >= 3
    }

    /// Index of the prompt-box top border line, if present.
    ///
    /// Ported from herdr's `prompt_box_top_border_index`: the second horizontal
    /// rule counting from the bottom of the visible grid.
    fn prompt_box_top_border_index(lines: &[String]) -> Option<usize> {
        let mut border_count = 0;
        for index in (0..lines.len()).rev() {
            if Self::is_horizontal_rule(&lines[index]) {
                border_count += 1;
                if border_count == 2 {
                    return Some(index);
                }
            }
        }
        None
    }
}

/// Character print observations accompanying a rendered terminal prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenObservation {
    /// Positive-width print callbacks since the previous observation.
    /// Zero-width changes are detected through the rendered text instead.
    pub drew_chars: bool,
}

impl ScreenObservation {
    fn new(drew_chars: bool) -> Self {
        Self { drew_chars }
    }
}

/// Print callbacks for one feed; parser state lives in `ScreenTracker`.
#[derive(Default)]
struct DrawObservation {
    drew_chars: bool,
}

impl vte::Perform for DrawObservation {
    fn print(&mut self, character: char) {
        if grid_print_width(character).is_some() {
            self.drew_chars = true;
        }
    }
}

/// Returns the width of a conservatively eligible character redraw.
///
/// `vt100` ignores replacement/C1 characters and can discard combining marks
/// at the cursor origin. Positive-width callbacks count even for an identical
/// repaint; actual zero-width cell changes remain covered by the text diff.
fn grid_print_width(character: char) -> Option<usize> {
    if character == '\u{fffd}' || ('\u{80}'..'\u{a0}').contains(&character) {
        return None;
    }
    character.width().filter(|width| *width > 0)
}

/// Print and completed-control-action callbacks for
/// [`ScreenTracker::feed_observed`]; parser state lives in `ScreenTracker`.
#[derive(Default)]
struct ObservedFeed {
    completed_action: bool,
    observe_before: bool,
    changes_text: bool,
    width: Option<usize>,
}

impl vte::Perform for ObservedFeed {
    fn print(&mut self, character: char) {
        self.changes_text = true;
        if let Some(width) = grid_print_width(character) {
            self.width = Some(width);
        }
    }

    fn execute(&mut self, byte: u8) {
        self.completed_action = true;
        self.observe_before = matches!(byte, 0x08..=0x0d);
        self.changes_text = matches!(byte, 0x0a..=0x0c);
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        _intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        self.completed_action = true;
        self.observe_before = action != 'm';
        // Mode changes affect visible text only when switching buffers.
        self.changes_text = match action {
            'h' | 'l' => params
                .iter()
                .any(|part| part.iter().any(|value| matches!(value, 47 | 1047 | 1049))),
            'm' | 'A'..='H' | 'f' | 'd' | 's' | 'u' | 'n' | 'c' | 'r' => false,
            _ => true,
        };
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, action: u8) {
        self.completed_action = true;
        self.observe_before = true;
        self.changes_text = matches!(action, b'D' | b'E' | b'M' | b'c');
    }

    fn osc_dispatch(&mut self, _params: &[&[u8]], _wide_osc: bool) {
        self.completed_action = true;
    }

    fn unhook(&mut self) {
        self.completed_action = true;
        self.observe_before = true;
        self.changes_text = true;
    }
}

#[cfg(test)]
mod tests {
    use super::{ScreenRegion, ScreenTracker};

    #[test]
    fn bottom_lines_returns_visible_tail() {
        let mut tracker = ScreenTracker::new(4, 20);

        tracker.feed(b"one\r\ntwo\r\nthree");

        for (name, count, expected) in [
            ("tail", 2, vec!["three", ""]),
            ("zero count", 0, vec![]),
            ("oversized count", 10, vec!["one", "two", "three", ""]),
        ] {
            assert_eq!(
                tracker.bottom_lines(count),
                ScreenRegion {
                    lines: expected.into_iter().map(str::to_owned).collect(),
                },
                "{name}"
            );
        }
    }

    #[test]
    fn bottom_non_empty_lines_skips_blank_rows() {
        let mut tracker = ScreenTracker::new(5, 20);

        tracker.feed(b"alpha\r\n\r\nbeta\r\n\r\ngamma");

        for (name, count, expected) in [
            ("tail", 2, vec!["beta", "gamma"]),
            ("zero count", 0, vec![]),
            ("oversized count", 10, vec!["alpha", "beta", "gamma"]),
        ] {
            assert_eq!(
                tracker.bottom_non_empty_lines(count),
                ScreenRegion {
                    lines: expected.into_iter().map(str::to_owned).collect(),
                },
                "{name}"
            );
        }
    }

    #[test]
    fn resizing_updates_visible_dimensions() {
        let mut tracker = ScreenTracker::new(2, 5);

        tracker.feed(b"hello");
        tracker.resize(3, 10);
        tracker.feed(b"\x1b[3;10H!");

        let lines = tracker.visible_lines();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "hello");
        assert_eq!(lines[2], "         !");
    }

    #[test]
    fn resizing_shrink_updates_visible_dimensions() {
        let mut tracker = ScreenTracker::new(2, 10);

        tracker.feed(b"abcdef");
        tracker.resize(2, 3);

        let lines = tracker.visible_lines();
        assert_eq!(lines.len(), 2);
        assert_eq!(tracker.slice_columns(0, 0, 10), "abc");
    }

    #[test]
    fn slice_columns_preserves_trailing_spaces() {
        let mut tracker = ScreenTracker::new(1, 5);

        tracker.feed(b"ab");

        assert_eq!(tracker.slice_columns(0, 0, 5), "ab   ");
    }

    #[test]
    fn cjk_wide_glyph_slicing_does_not_duplicate_or_split_glyphs() {
        let mut tracker = ScreenTracker::new(1, 10);

        tracker.feed("a界b".as_bytes());

        assert_eq!(tracker.slice_columns(0, 0, 4), "a界b");
        assert_eq!(tracker.slice_columns(0, 1, 1), " ");
        assert_eq!(tracker.slice_columns(0, 1, 2), "界");
        assert_eq!(tracker.slice_columns(0, 2, 1), " ");
        assert_eq!(tracker.slice_columns(0, 2, 2), " b");
        assert_eq!(tracker.slice_columns(0, 3, 1), "b");
    }

    #[test]
    fn recent_text_joins_visible_lines_without_trailing_blank_rows() {
        let mut tracker = ScreenTracker::new(4, 20);

        tracker.feed(b"build\r\nrunning");

        assert_eq!(tracker.recent_text(), "build\nrunning");
    }

    #[test]
    fn after_last_prompt_marker_returns_text_following_the_marker() {
        let mut tracker = ScreenTracker::new(4, 40);

        tracker.feed("preamble\r\n\u{203a} run tests\r\napproval required".as_bytes());

        assert_eq!(tracker.after_last_prompt_marker(), "approval required\n");
    }

    #[test]
    fn after_last_prompt_marker_returns_whole_text_when_no_marker_present() {
        let mut tracker = ScreenTracker::new(3, 40);

        tracker.feed(b"no prompt here\r\njust output");

        assert_eq!(
            tracker.after_last_prompt_marker(),
            "no prompt here\njust output\n"
        );
    }

    #[test]
    fn after_last_horizontal_rule_returns_text_after_full_line_rule() {
        let mut tracker = ScreenTracker::new(4, 40);

        tracker.feed("header\r\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\r\nbody text".as_bytes());

        assert_eq!(tracker.after_last_horizontal_rule(), "body text\n");
    }

    #[test]
    fn after_last_horizontal_rule_treats_short_prefixed_run_as_rule() {
        let mut tracker = ScreenTracker::new(3, 40);

        // A leading run of three `─` followed by other text still counts as a
        // rule because the run length is >= 3.
        tracker.feed("\u{2500}\u{2500}\u{2500} divider\r\nafter divider".as_bytes());

        assert_eq!(tracker.after_last_horizontal_rule(), "after divider\n");
    }

    #[test]
    fn after_last_horizontal_rule_returns_whole_text_when_no_rule_present() {
        let mut tracker = ScreenTracker::new(3, 40);

        tracker.feed(b"first line\r\nsecond line");

        assert_eq!(
            tracker.after_last_horizontal_rule(),
            "first line\nsecond line\n"
        );
    }

    #[test]
    fn prompt_box_body_returns_text_between_the_two_bottom_rules() {
        let mut tracker = ScreenTracker::new(5, 40);

        // Top border, prompt body, bottom border, then a hint line.
        tracker.feed(
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\r\n\u{203a} type here\r\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\r\nesc to cancel".as_bytes(),
        );

        assert_eq!(tracker.prompt_box_body(), "\u{203a} type here");
    }

    #[test]
    fn prompt_box_body_returns_empty_when_fewer_than_two_rules_present() {
        let mut tracker = ScreenTracker::new(3, 40);

        tracker.feed("\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\r\nonly one rule".as_bytes());

        assert_eq!(tracker.prompt_box_body(), "");
    }

    #[test]
    fn top_non_empty_lines_returns_requested_head_without_blank_rows() {
        let mut tracker = ScreenTracker::new(6, 20);

        tracker.feed(b"first\r\n\r\nsecond\r\n\r\nthird");

        assert_eq!(
            tracker.top_non_empty_lines(2).lines,
            vec!["first".to_string(), "second".to_string()]
        );
    }

    #[test]
    fn top_non_empty_lines_returns_all_available_non_empty_lines_when_fewer_exist() {
        let mut tracker = ScreenTracker::new(3, 20);

        tracker.feed(b"\r\nonly content");

        assert_eq!(tracker.top_non_empty_lines(2).lines, vec!["only content"]);
    }

    #[test]
    fn top_non_empty_lines_preserves_cjk_wide_glyph_rows() {
        let mut tracker = ScreenTracker::new(3, 20);

        tracker.feed("\r\n信任仓库界".as_bytes());

        assert_eq!(
            tracker.top_non_empty_lines(2).lines,
            vec!["信任仓库界".to_string()]
        );
    }

    #[test]
    fn last_non_empty_above_prompt_box_returns_nearest_content_line() {
        let mut tracker = ScreenTracker::new(7, 40);

        tracker.feed(
            "older status\r\nlatest status\r\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\r\n\u{203a} type here\r\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}".as_bytes(),
        );

        assert_eq!(tracker.last_non_empty_above_prompt_box(), "latest status");
    }

    #[test]
    fn last_non_empty_above_prompt_box_preserves_cjk_wide_glyph_row() {
        let mut tracker = ScreenTracker::new(6, 40);

        tracker.feed(
            "审批需要确认界\r\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\r\n\u{203a} type here\r\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}"
                .as_bytes(),
        );

        assert_eq!(tracker.last_non_empty_above_prompt_box(), "审批需要确认界");
    }

    #[test]
    fn last_non_empty_above_prompt_box_is_empty_without_a_complete_box() {
        let mut tracker = ScreenTracker::new(3, 40);

        tracker.feed("status\r\n\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}".as_bytes());

        assert_eq!(tracker.last_non_empty_above_prompt_box(), "");
    }

    #[test]
    fn reset_clears_the_visible_grid() {
        let mut tracker = ScreenTracker::new(3, 20);

        tracker.feed(b"hello\r\nworld");
        assert_eq!(tracker.recent_text(), "hello\nworld");

        tracker.reset();

        assert_eq!(tracker.recent_text(), "");
        assert_eq!(
            tracker.visible_lines(),
            vec![String::new(), String::new(), String::new()]
        );
    }

    #[test]
    fn reset_preserves_screen_dimensions() {
        let mut tracker = ScreenTracker::new(4, 25);

        tracker.feed(b"content");
        tracker.reset();

        assert_eq!(tracker.visible_lines().len(), 4);
        assert_eq!(tracker.slice_columns(0, 0, 25).len(), 25);
    }
}
