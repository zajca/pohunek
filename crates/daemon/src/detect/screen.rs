const SCROLLBACK_LINES: usize = 0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenRegion {
    pub lines: Vec<String>,
}

pub struct ScreenTracker {
    parser: vt100::Parser,
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
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
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
    }

    #[must_use]
    pub fn visible_lines(&self) -> Vec<String> {
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

    /// Returns a raw terminal-column slice with blank cells preserved.
    ///
    /// Wide glyphs are emitted only when the requested range includes both
    /// terminal cells. Clipped wide glyph cells and continuation cells are
    /// represented with spaces so region geometry remains stable.
    #[must_use]
    pub fn slice_columns(&self, row: u16, start_col: u16, width: u16) -> String {
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

#[cfg(test)]
mod tests {
    use super::{ScreenRegion, ScreenTracker};

    #[test]
    fn feeding_plain_output_produces_visible_lines() {
        let mut tracker = ScreenTracker::new(3, 20);

        tracker.feed(b"hello\r\nworld");

        assert_eq!(
            tracker.visible_lines(),
            vec!["hello".to_string(), "world".to_string(), String::new()]
        );
    }

    #[test]
    fn bottom_lines_returns_visible_tail() {
        let mut tracker = ScreenTracker::new(4, 20);

        tracker.feed(b"one\r\ntwo\r\nthree\r\nfour");

        assert_eq!(
            tracker.bottom_lines(2),
            ScreenRegion {
                lines: vec!["three".to_string(), "four".to_string()],
            }
        );
    }

    #[test]
    fn bottom_lines_zero_count_returns_empty_region() {
        let mut tracker = ScreenTracker::new(3, 20);

        tracker.feed(b"one\r\ntwo");

        assert_eq!(tracker.bottom_lines(0), ScreenRegion { lines: Vec::new() });
    }

    #[test]
    fn bottom_lines_oversized_count_returns_all_visible_lines() {
        let mut tracker = ScreenTracker::new(3, 20);

        tracker.feed(b"one\r\ntwo");

        assert_eq!(
            tracker.bottom_lines(10),
            ScreenRegion {
                lines: vec!["one".to_string(), "two".to_string(), String::new()],
            }
        );
    }

    #[test]
    fn bottom_non_empty_lines_skips_blank_rows() {
        let mut tracker = ScreenTracker::new(5, 20);

        tracker.feed(b"alpha\r\n\r\nbeta\r\n\r\ngamma");

        assert_eq!(
            tracker.bottom_non_empty_lines(2),
            ScreenRegion {
                lines: vec!["beta".to_string(), "gamma".to_string()],
            }
        );
    }

    #[test]
    fn bottom_non_empty_lines_zero_count_returns_empty_region() {
        let mut tracker = ScreenTracker::new(3, 20);

        tracker.feed(b"one\r\ntwo");

        assert_eq!(
            tracker.bottom_non_empty_lines(0),
            ScreenRegion { lines: Vec::new() }
        );
    }

    #[test]
    fn bottom_non_empty_lines_oversized_count_returns_all_non_empty_lines() {
        let mut tracker = ScreenTracker::new(5, 20);

        tracker.feed(b"alpha\r\n\r\nbeta\r\n\r\ngamma");

        assert_eq!(
            tracker.bottom_non_empty_lines(10),
            ScreenRegion {
                lines: vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()],
            }
        );
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
    fn slice_columns_returns_spaces_for_blank_region() {
        let tracker = ScreenTracker::new(1, 6);

        assert_eq!(tracker.slice_columns(0, 1, 3), "   ");
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
