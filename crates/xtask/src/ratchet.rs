//! Lexical helpers shared by the test-code ratchets (`timing_ratchet`,
//! `timing_deadline_ratchet`, `sdk_timing_ratchet`).
//!
//! Each ratchet counts one construct per file in test code against a committed
//! baseline that may only go down, and accepts a `// timing-allowed: #<issue>
//! <reason>` line comment as an escape hatch. The scanners work on the blanked
//! views of [`crate::test_code::Views`]; this module holds what they share:
//! bracket and identifier scanning, marker parsing and the baseline file
//! format.

// Rust guideline compliant 2026-10-01

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::test_code::{is_ident, skip_whitespace, SourceFile};

/// Index of the byte matching the opener at `open` in `bytes`, or the length.
#[must_use]
pub fn matching_close(bytes: &[u8], open: usize) -> usize {
    let (opener, closer) = match bytes[open] {
        b'[' => (b'[', b']'),
        b'(' => (b'(', b')'),
        _ => (b'{', b'}'),
    };
    let mut depth = 0_usize;
    for (i, &byte) in bytes.iter().enumerate().skip(open) {
        if byte == opener {
            depth += 1;
        } else if byte == closer {
            depth -= 1;
            if depth == 0 {
                return i;
            }
        }
    }
    bytes.len()
}

/// Index one past the identifier that starts at `start`.
#[must_use]
pub fn end_of_ident(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    while bytes.get(end).copied().is_some_and(is_ident) {
        end += 1;
    }
    end
}

/// Whether `bytes[offset..]` starts an identifier-bounded word.
#[must_use]
pub fn starts_word(bytes: &[u8], offset: usize) -> bool {
    offset == 0 || !is_ident(bytes[offset - 1])
}

/// The identifier that ends right before `end`, skipping whitespace first.
#[must_use]
pub fn ident_before(bytes: &[u8], end: usize) -> &str {
    let mut stop = end;
    while stop > 0 && bytes[stop - 1].is_ascii_whitespace() {
        stop -= 1;
    }
    let mut start = stop;
    while start > 0 && is_ident(bytes[start - 1]) {
        start -= 1;
    }
    std::str::from_utf8(&bytes[start..stop]).unwrap_or("")
}

/// Index just past `token` when it follows `from` after optional whitespace.
#[must_use]
pub fn expect_token(bytes: &[u8], from: usize, token: &str) -> Option<usize> {
    let at = skip_whitespace(bytes, from);
    bytes[at..]
        .starts_with(token.as_bytes())
        .then_some(at + token.len())
}

/// Whether `text` contains `word` bounded by non-identifier bytes.
#[must_use]
pub fn contains_word(text: &str, word: &str) -> bool {
    let bytes = text.as_bytes();
    text.match_indices(word).any(|(at, _)| {
        starts_word(bytes, at) && !bytes.get(at + word.len()).copied().is_some_and(is_ident)
    })
}

/// Splits `body` at top-level commas.
#[must_use]
pub fn top_level_arguments(body: &str) -> Vec<&str> {
    let mut depth = 0_usize;
    let mut start = 0;
    let mut arguments = Vec::new();
    for (i, c) in body.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                arguments.push(&body[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    arguments.push(&body[start..]);
    arguments
}

/// Byte offsets of the `//` that begin a line comment, outside block comments,
/// strings and char literals.
///
/// `Views::code` blanks line and block comments alike, so a blanked,
/// non-whitespace byte starts a comment and the text there tells the kind.
#[must_use]
pub fn line_comment_starts(text: &str, code: &str) -> Vec<usize> {
    let (text, code) = (text.as_bytes(), code.as_bytes());
    let mut starts = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if code[i] != b' ' || text[i].is_ascii_whitespace() {
            i += 1;
        } else if text[i..].starts_with(b"//") {
            starts.push(i);
            i += text[i..]
                .iter()
                .position(|&b| b == b'\n')
                .unwrap_or(text.len() - i);
        } else {
            let mut depth = 0_usize;
            while i < text.len() {
                if text[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if text[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
        }
    }
    starts
}

/// Lines (one-based) carrying a valid `marker` comment and the malformed
/// marker lines of `file`, whose text is `text`.
#[must_use]
pub fn markers(file: &SourceFile, text: &str, marker: &str) -> (Vec<usize>, Vec<usize>) {
    let mut valid = Vec::new();
    let mut malformed = Vec::new();
    for at in line_comment_starts(text, &file.views().code) {
        let rest = &text[at + 2..];
        let rest = &rest[..rest.find('\n').unwrap_or(rest.len())];
        if rest.starts_with(['/', '!']) {
            continue;
        }
        if let Some(body) = rest.trim_start().strip_prefix(marker) {
            let line = file.line_of(at);
            if is_valid_marker(body) {
                valid.push(line);
            } else {
                malformed.push(line);
            }
        }
    }
    (valid, malformed)
}

/// Whether the text after the marker keyword is `: #<digits> <reason>`.
#[must_use]
pub fn is_valid_marker(body: &str) -> bool {
    let Some(rest) = body.strip_prefix(':') else {
        return false;
    };
    let Some(rest) = rest.trim().strip_prefix('#') else {
        return false;
    };
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    digits > 0
        && rest[digits..].starts_with(char::is_whitespace)
        && !rest[digits..].trim().is_empty()
}

/// Attribute text (whitespace removed) that puts a test on virtual time.
const PAUSED_ATTRIBUTE: &str = "start_paused=true";

/// Whether the byte `offset` sits in a test item declared on virtual time
/// (`#[tokio::test(.. start_paused = true ..)]`).
#[must_use]
pub fn on_virtual_time(file: &SourceFile, offset: usize) -> bool {
    file.test_attributes_at(offset).iter().any(|attribute| {
        let squeezed: String = attribute.chars().filter(|c| !c.is_whitespace()).collect();
        squeezed.contains(PAUSED_ATTRIBUTE)
    })
}

/// Whether `line` is exempted by a valid marker on it or on the line above.
#[must_use]
pub fn is_allowed(valid_marker_lines: &[usize], line: usize) -> bool {
    valid_marker_lines.contains(&line) || valid_marker_lines.contains(&line.saturating_sub(1))
}

/// Parses the baseline text: `<path> <count>` per line, `#` starts a comment
/// line.
///
/// # Panics
///
/// Panics, naming the line, on a line that is not `<path> <count>` and on a
/// duplicate path.
#[must_use]
pub fn parse_baseline(text: &str) -> BTreeMap<String, usize> {
    let mut baseline = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (path, count) = line
            .rsplit_once(' ')
            .unwrap_or_else(|| panic!("baseline line {}: expected `<path> <count>`", index + 1));
        let count = count
            .parse()
            .unwrap_or_else(|e| panic!("baseline line {}: bad count: {e}", index + 1));
        assert!(
            baseline.insert(path.trim().to_owned(), count).is_none(),
            "baseline line {}: duplicate path {path}",
            index + 1
        );
    }
    baseline
}

/// Renders `counts` as baseline file text under `header`.
#[must_use]
pub fn format_baseline(header: &str, counts: &BTreeMap<String, usize>) -> String {
    let mut text = header.to_owned();
    for (path, count) in counts {
        writeln!(text, "{path} {count}").expect("writing to a String cannot fail");
    }
    text
}
