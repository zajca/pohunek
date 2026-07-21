//! Unified-diff parsing shared by `session.diff` and `gh pr diff` output.

// Rust guideline compliant 2026-07-19

/// One parsed unified diff: the ordered list of files it touches.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffModel {
    /// Files touched by the diff, in the order they appear in the source text.
    pub files: Vec<DiffFile>,
}

/// One file's diff: its path, change kind, and hunks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFile {
    /// Current path (the pre-image path for a deleted file).
    pub path: String,
    /// How this file changed.
    pub status: DiffFileStatus,
    /// Hunks of changed lines. Empty for a pure rename, a mode-only change,
    /// or a binary file.
    pub hunks: Vec<DiffHunk>,
}

/// How one file changed between the diff's two sides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffFileStatus {
    /// Content changed (including a mode-only change, which has no hunks).
    Modified,
    /// The file exists only on the new side (including an untracked file
    /// diffed against `/dev/null`).
    Added,
    /// The file exists only on the old side.
    Deleted,
    /// The file moved from `old_path` to [`DiffFile::path`].
    Renamed {
        /// Path before the rename.
        old_path: String,
    },
    /// Git reported this file as binary (`Binary files ... differ`); no
    /// textual hunks are available.
    ///
    /// Literal `GIT binary patch` payload (emitted only with `git diff
    /// --binary`) is not parsed: neither `session.diff` (plain `git diff
    /// --no-color`, no `--binary`) nor `gh pr diff` requests it, so it is
    /// never expected on either supported source. If it ever appeared, the
    /// patch body lines match no recognized line prefix and are silently
    /// skipped, leaving the file binary with zero hunks — graceful
    /// degradation, not a panic.
    Binary,
}

/// One `@@ ... @@` hunk and its lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    /// Full hunk header line, including the optional trailing section heading.
    pub header: String,
    /// Lines in this hunk, in source order.
    pub lines: Vec<DiffLine>,
}

/// One line inside a hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    /// Whether this line is context, an addition, or a removal.
    pub kind: DiffLineKind,
    /// Line number on the old side, or `None` for an added line.
    pub old_line: Option<u32>,
    /// Line number on the new side, or `None` for a removed line.
    pub new_line: Option<u32>,
    /// Line text with the leading `+`/`-`/` ` marker stripped.
    pub text: String,
    /// Whether a following `\ No newline at end of file` marker applied to
    /// this line.
    pub no_newline_at_eof: bool,
}

/// Kind of one hunk line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    /// Unchanged line shown for context.
    Context,
    /// Line present only on the new side.
    Add,
    /// Line present only on the old side.
    Remove,
}

/// Parses unified-diff text into a [`DiffModel`].
///
/// Accepts identically shaped text from `session.diff`'s daemon-generated
/// `git diff`/`git diff --no-index` output and from `gh pr diff` — both emit
/// the same unified-diff format, so this is the single parser for both
/// sources. Handles renamed, added (including untracked-as-added via
/// `--- /dev/null`), deleted, binary, and mode-change-only files, plus a
/// `\ No newline at end of file` marker.
///
/// Never panics. The daemon promises `session.diff` truncation only cuts at a
/// *file* boundary, but this parser also serves `gh pr diff`, which makes no
/// such promise, so it is defensive either way: a hunk cut short mid-file
/// simply yields whatever lines were present before the cut (each line access
/// is bounds-checked, never indexed unconditionally), and every file
/// preceding the cut is kept in full.
#[must_use]
pub fn parse_unified_diff(text: &str) -> DiffModel {
    let lines: Vec<&str> = text.lines().collect();
    let mut files = Vec::new();
    let mut index = 0;

    while index < lines.len() {
        if !lines[index].starts_with("diff --git ") {
            index += 1;
            continue;
        }
        let diff_git_line = lines[index];
        index += 1;
        let (mut old_path, mut new_path) = parse_diff_git_line(diff_git_line).unwrap_or_default();
        let (is_rename, is_binary) =
            parse_file_header(&lines, &mut index, &mut old_path, &mut new_path);
        let hunks = parse_hunks(&lines, &mut index);

        let status = diff_file_status(&old_path, &new_path, is_rename, is_binary);
        let path = canonical_path(&old_path, &new_path);
        if !path.is_empty() {
            files.push(DiffFile {
                path,
                status,
                hunks,
            });
        }
    }

    DiffModel { files }
}

/// Consumes one file segment's header lines (index/mode/rename/`---`/`+++`/
/// binary), stopping at the first hunk header or the next file. Updates
/// `old_path`/`new_path` in place as more specific lines override the
/// `diff --git` line's fallback values. Returns `(is_rename, is_binary)`.
fn parse_file_header(
    lines: &[&str],
    index: &mut usize,
    old_path: &mut String,
    new_path: &mut String,
) -> (bool, bool) {
    let mut is_rename = false;
    let mut is_binary = false;
    while *index < lines.len() {
        let line = lines[*index];
        if line.starts_with("diff --git ") || line.starts_with("@@ ") {
            break;
        }
        *index += 1;
        if let Some(path) = line.strip_prefix("rename from ") {
            path.clone_into(old_path);
            is_rename = true;
        } else if let Some(path) = line.strip_prefix("rename to ") {
            path.clone_into(new_path);
            is_rename = true;
        } else if let Some(path) = line.strip_prefix("--- ") {
            *old_path = strip_ab_prefix(path, "a/");
        } else if let Some(path) = line.strip_prefix("+++ ") {
            *new_path = strip_ab_prefix(path, "b/");
        } else if let Some(rest) = line
            .strip_prefix("Binary files ")
            .and_then(|rest| rest.strip_suffix(" differ"))
        {
            is_binary = true;
            if let Some((left, right)) = rest.split_once(" and ") {
                *old_path = strip_ab_prefix(left, "a/");
                *new_path = strip_ab_prefix(right, "b/");
            }
        }
    }
    (is_rename, is_binary)
}

/// Consumes one file segment's `@@ ... @@` hunks and their +/-/context lines,
/// stopping at the next file. Never panics on a hunk cut short mid-file: a
/// trailing hunk with fewer lines than its header claims is still returned
/// with whatever lines were present.
fn parse_hunks(lines: &[&str], index: &mut usize) -> Vec<DiffHunk> {
    let mut hunks = Vec::new();
    let mut current: Option<(DiffHunk, u32, u32)> = None;
    while *index < lines.len() {
        let line = lines[*index];
        if line.starts_with("diff --git ") {
            break;
        }
        *index += 1;
        if let Some(header) = line.strip_prefix("@@ ") {
            if let Some((hunk, ..)) = current.take() {
                hunks.push(hunk);
            }
            let (old_start, new_start) = parse_hunk_start(header).unwrap_or((0, 0));
            current = Some((
                DiffHunk {
                    header: line.to_owned(),
                    lines: Vec::new(),
                },
                old_start,
                new_start,
            ));
            continue;
        }
        let Some((hunk, old_cursor, new_cursor)) = current.as_mut() else {
            // Stray line before any hunk header in this file (shouldn't
            // happen for well-formed input); ignore rather than panic.
            continue;
        };
        if line.starts_with('\\') {
            if let Some(last) = hunk.lines.last_mut() {
                last.no_newline_at_eof = true;
            }
            continue;
        }
        push_hunk_line(hunk, line, old_cursor, new_cursor);
    }
    if let Some((hunk, ..)) = current.take() {
        hunks.push(hunk);
    }
    hunks
}

/// Classifies and appends one hunk body line, advancing the old/new line
/// cursors that track its position on each side.
fn push_hunk_line(hunk: &mut DiffHunk, line: &str, old_cursor: &mut u32, new_cursor: &mut u32) {
    let (kind, body) = if let Some(body) = line.strip_prefix('+') {
        (DiffLineKind::Add, body)
    } else if let Some(body) = line.strip_prefix('-') {
        (DiffLineKind::Remove, body)
    } else if let Some(body) = line.strip_prefix(' ') {
        (DiffLineKind::Context, body)
    } else if line.is_empty() {
        (DiffLineKind::Context, line)
    } else {
        return;
    };
    let (old_line, new_line) = match kind {
        DiffLineKind::Add => {
            let value = *new_cursor;
            *new_cursor += 1;
            (None, Some(value))
        }
        DiffLineKind::Remove => {
            let value = *old_cursor;
            *old_cursor += 1;
            (Some(value), None)
        }
        DiffLineKind::Context => {
            let old_value = *old_cursor;
            let new_value = *new_cursor;
            *old_cursor += 1;
            *new_cursor += 1;
            (Some(old_value), Some(new_value))
        }
    };
    hunk.lines.push(DiffLine {
        kind,
        old_line,
        new_line,
        text: body.to_owned(),
        no_newline_at_eof: false,
    });
}

/// Extracts the fallback `(old_path, new_path)` pair from a `diff --git a/X
/// b/Y` line. Overridden by `--- `/`+++ `/rename/binary lines when present;
/// this is only the fallback for the rare segment that has none of those
/// (e.g. a mode-only change).
fn parse_diff_git_line(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("diff --git ")?;
    let rest = rest.strip_prefix("a/").unwrap_or(rest);
    // Paths are not `diff --git`-escaped for spaces, so this is a heuristic:
    // find the last " b/" separator. A path literally containing " b/" would
    // mis-split here, same ambiguity every line-based diff parser accepts.
    let split_at = rest.rfind(" b/")?;
    Some((rest[..split_at].to_owned(), rest[split_at + 3..].to_owned()))
}

fn strip_ab_prefix(path: &str, prefix: &str) -> String {
    path.strip_prefix(prefix).unwrap_or(path).to_owned()
}

/// Parses the old/new start line numbers from a hunk header's text after the
/// leading `"@@ "`, e.g. `"-12,7 +12,9 @@ fn foo() {"`.
fn parse_hunk_start(header_rest: &str) -> Option<(u32, u32)> {
    let rest = header_rest.strip_prefix('-')?;
    let (old_part, rest) = rest.split_once(' ')?;
    let rest = rest.strip_prefix('+')?;
    let new_part = rest.split_whitespace().next()?;
    Some((parse_range_start(old_part)?, parse_range_start(new_part)?))
}

fn parse_range_start(part: &str) -> Option<u32> {
    part.split(',').next()?.parse().ok()
}

fn diff_file_status(
    old_path: &str,
    new_path: &str,
    is_rename: bool,
    is_binary: bool,
) -> DiffFileStatus {
    if is_rename && old_path != new_path {
        return DiffFileStatus::Renamed {
            old_path: old_path.to_owned(),
        };
    }
    if is_binary {
        return DiffFileStatus::Binary;
    }
    if old_path == "/dev/null" {
        return DiffFileStatus::Added;
    }
    if new_path == "/dev/null" {
        return DiffFileStatus::Deleted;
    }
    DiffFileStatus::Modified
}

fn canonical_path(old_path: &str, new_path: &str) -> String {
    if !new_path.is_empty() && new_path != "/dev/null" {
        new_path.to_owned()
    } else if !old_path.is_empty() {
        old_path.to_owned()
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_unified_diff, DiffFileStatus, DiffLineKind};

    /// Joins fixture lines with `\n` and a trailing `\n`, exactly like real
    /// diff text.
    ///
    /// Deliberately not built with `\`-continued string literals: Rust's
    /// backslash-newline escape strips *all* leading whitespace on the
    /// following line, which silently eats a fixture's literal leading
    /// `' '` context-line marker. Building it from a `&[&str]` slice keeps
    /// every line's content exactly as written.
    fn diff_text(lines: &[&str]) -> String {
        let mut text = lines.join("\n");
        text.push('\n');
        text
    }

    #[test]
    fn empty_diff_text_yields_no_files() {
        let model = parse_unified_diff("");
        assert!(model.files.is_empty());
    }

    #[test]
    fn modified_file_reports_context_add_and_remove_lines_with_numbers() {
        let text = diff_text(&[
            "diff --git a/src/lib.rs b/src/lib.rs",
            "index abc123..def456 100644",
            "--- a/src/lib.rs",
            "+++ b/src/lib.rs",
            "@@ -1,3 +1,4 @@",
            " context line",
            "-old line",
            "+new line",
            "+another new line",
        ]);

        let model = parse_unified_diff(&text);

        assert_eq!(model.files.len(), 1);
        let file = &model.files[0];
        assert_eq!(file.path, "src/lib.rs");
        assert_eq!(file.status, DiffFileStatus::Modified);
        assert_eq!(file.hunks.len(), 1);
        let hunk = &file.hunks[0];
        assert_eq!(hunk.header, "@@ -1,3 +1,4 @@");
        assert_eq!(hunk.lines.len(), 4);
        assert_eq!(hunk.lines[0].kind, DiffLineKind::Context);
        assert_eq!(hunk.lines[0].old_line, Some(1));
        assert_eq!(hunk.lines[0].new_line, Some(1));
        assert_eq!(hunk.lines[0].text, "context line");
        assert_eq!(hunk.lines[1].kind, DiffLineKind::Remove);
        assert_eq!(hunk.lines[1].old_line, Some(2));
        assert_eq!(hunk.lines[1].new_line, None);
        assert_eq!(hunk.lines[1].text, "old line");
        assert_eq!(hunk.lines[2].kind, DiffLineKind::Add);
        assert_eq!(hunk.lines[2].old_line, None);
        assert_eq!(hunk.lines[2].new_line, Some(2));
        assert_eq!(hunk.lines[2].text, "new line");
        assert_eq!(hunk.lines[3].new_line, Some(3));
    }

    #[test]
    fn untracked_file_diffed_against_dev_null_is_added() {
        let text = diff_text(&[
            "diff --git a/dev/null b/new_file.txt",
            "new file mode 100644",
            "index 0000000..abcd123",
            "--- /dev/null",
            "+++ b/new_file.txt",
            "@@ -0,0 +1,2 @@",
            "+line one",
            "+line two",
        ]);

        let model = parse_unified_diff(&text);

        assert_eq!(model.files.len(), 1);
        let file = &model.files[0];
        assert_eq!(file.path, "new_file.txt");
        assert_eq!(file.status, DiffFileStatus::Added);
        assert_eq!(file.hunks[0].lines.len(), 2);
    }

    #[test]
    fn deleted_file_reports_the_old_path_and_deleted_status() {
        let text = diff_text(&[
            "diff --git a/removed.txt b/removed.txt",
            "deleted file mode 100644",
            "index abc123..0000000",
            "--- a/removed.txt",
            "+++ /dev/null",
            "@@ -1,2 +0,0 @@",
            "-line one",
            "-line two",
        ]);

        let model = parse_unified_diff(&text);

        assert_eq!(model.files.len(), 1);
        let file = &model.files[0];
        assert_eq!(file.path, "removed.txt");
        assert_eq!(file.status, DiffFileStatus::Deleted);
        assert_eq!(file.hunks[0].lines[0].old_line, Some(1));
        assert_eq!(file.hunks[0].lines[0].new_line, None);
    }

    #[test]
    fn renamed_file_with_full_similarity_has_no_hunks() {
        let text = diff_text(&[
            "diff --git a/old_name.txt b/new_name.txt",
            "similarity index 100%",
            "rename from old_name.txt",
            "rename to new_name.txt",
        ]);

        let model = parse_unified_diff(&text);

        assert_eq!(model.files.len(), 1);
        let file = &model.files[0];
        assert_eq!(file.path, "new_name.txt");
        assert_eq!(
            file.status,
            DiffFileStatus::Renamed {
                old_path: "old_name.txt".to_owned()
            }
        );
        assert!(file.hunks.is_empty());
    }

    #[test]
    fn renamed_file_with_content_changes_has_hunks() {
        let text = diff_text(&[
            "diff --git a/old_name.txt b/new_name.txt",
            "similarity index 80%",
            "rename from old_name.txt",
            "rename to new_name.txt",
            "index abc123..def456 100644",
            "--- a/old_name.txt",
            "+++ b/new_name.txt",
            "@@ -1,1 +1,1 @@",
            "-old content",
            "+new content",
        ]);

        let model = parse_unified_diff(&text);

        let file = &model.files[0];
        assert_eq!(
            file.status,
            DiffFileStatus::Renamed {
                old_path: "old_name.txt".to_owned()
            }
        );
        assert_eq!(file.hunks[0].lines.len(), 2);
    }

    #[test]
    fn binary_file_reports_binary_status_with_no_hunks() {
        let text = diff_text(&[
            "diff --git a/image.png b/image.png",
            "index abc123..def456 100644",
            "Binary files a/image.png and b/image.png differ",
        ]);

        let model = parse_unified_diff(&text);

        assert_eq!(model.files.len(), 1);
        let file = &model.files[0];
        assert_eq!(file.path, "image.png");
        assert_eq!(file.status, DiffFileStatus::Binary);
        assert!(file.hunks.is_empty());
    }

    #[test]
    fn binary_added_file_resolves_path_from_the_binary_stanza() {
        let text = diff_text(&[
            "diff --git a/asset.bin b/asset.bin",
            "new file mode 100644",
            "index 0000000..abcd123",
            "Binary files /dev/null and b/asset.bin differ",
        ]);

        let model = parse_unified_diff(&text);

        let file = &model.files[0];
        assert_eq!(file.path, "asset.bin");
        assert_eq!(file.status, DiffFileStatus::Binary);
    }

    #[test]
    fn mode_change_only_file_is_modified_with_no_hunks() {
        let text = diff_text(&[
            "diff --git a/script.sh b/script.sh",
            "old mode 100644",
            "new mode 100755",
        ]);

        let model = parse_unified_diff(&text);

        assert_eq!(model.files.len(), 1);
        let file = &model.files[0];
        assert_eq!(file.path, "script.sh");
        assert_eq!(file.status, DiffFileStatus::Modified);
        assert!(file.hunks.is_empty());
    }

    #[test]
    fn no_newline_at_eof_marker_flags_the_preceding_line() {
        let text = diff_text(&[
            "diff --git a/no_newline.txt b/no_newline.txt",
            "index abc123..def456 100644",
            "--- a/no_newline.txt",
            "+++ b/no_newline.txt",
            "@@ -1,1 +1,1 @@",
            "-old",
            "\\ No newline at end of file",
            "+new",
            "\\ No newline at end of file",
        ]);

        let model = parse_unified_diff(&text);

        let lines = &model.files[0].hunks[0].lines;
        assert_eq!(lines.len(), 2);
        assert!(lines[0].no_newline_at_eof);
        assert!(lines[1].no_newline_at_eof);
    }

    #[test]
    fn truncated_at_file_boundary_keeps_only_the_complete_leading_files() {
        let full = diff_text(&[
            "diff --git a/first.txt b/first.txt",
            "index abc123..def456 100644",
            "--- a/first.txt",
            "+++ b/first.txt",
            "@@ -1,1 +1,1 @@",
            "-old",
            "+new",
            "diff --git a/second.txt b/second.txt",
            "index abc123..def456 100644",
            "--- a/second.txt",
            "+++ b/second.txt",
            "@@ -1,1 +1,1 @@",
            "-old2",
            "+new2",
        ]);
        let file_boundary = full.find("diff --git a/second.txt").expect("file boundary");
        let truncated = &full[..file_boundary];

        let model = parse_unified_diff(truncated);

        assert_eq!(model.files.len(), 1);
        assert_eq!(model.files[0].path, "first.txt");
    }

    #[test]
    fn cut_mid_hunk_does_not_panic_and_keeps_the_preceding_complete_file() {
        let full = diff_text(&[
            "diff --git a/first.txt b/first.txt",
            "index abc123..def456 100644",
            "--- a/first.txt",
            "+++ b/first.txt",
            "@@ -1,1 +1,1 @@",
            "-old",
            "+new",
            "diff --git a/second.txt b/second.txt",
            "index abc123..def456 100644",
            "--- a/second.txt",
            "+++ b/second.txt",
            "@@ -1,5 +1,5 @@",
            "-line one of five claimed",
        ]);
        // `full` already cuts off mid-hunk (the header claims 5 old/new lines
        // but only one removal line follows): assert parsing the whole thing
        // does not panic, keeps `first.txt` complete, and keeps `second.txt`
        // with only the partial hunk data actually present.
        let model = parse_unified_diff(&full);

        assert_eq!(model.files.len(), 2);
        assert_eq!(model.files[0].path, "first.txt");
        assert_eq!(model.files[0].hunks[0].lines.len(), 2);
        assert_eq!(model.files[1].path, "second.txt");
        assert_eq!(model.files[1].hunks[0].lines.len(), 1);
    }

    #[test]
    fn gh_pr_diff_style_text_parses_identically_to_session_diff_text() {
        // `gh pr diff` emits the same unified-diff format as `git diff`; this
        // is a golden fixture in that style to prove the one parser serves
        // both sources identically.
        let gh_style = diff_text(&[
            "diff --git a/README.md b/README.md",
            "index 1111111..2222222 100644",
            "--- a/README.md",
            "+++ b/README.md",
            "@@ -1,2 +1,3 @@",
            " # Title",
            "+New line from the PR",
            " Existing line",
        ]);

        let model = parse_unified_diff(&gh_style);

        assert_eq!(model.files.len(), 1);
        assert_eq!(model.files[0].path, "README.md");
        assert_eq!(model.files[0].status, DiffFileStatus::Modified);
        assert_eq!(model.files[0].hunks[0].lines.len(), 3);
    }
}
