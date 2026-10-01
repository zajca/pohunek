//! Source scanning support: which regions of the workspace's Rust sources are
//! test code.
//!
//! Scans that apply only to tests (compile-time artifact lookups, sleeps,
//! host-state access) share this classifier so they agree on what a test is.
//! [`read_sources`] loads the workspace's `.rs` files and [`classify`] turns
//! them into [`SourceFile`]s that answer whether a byte offset is test code
//! and expose two aligned views of the text (see [`Views`]).
//!
//! Counted as test code:
//! - every `.rs` file under a `tests` directory, files named `tests.rs` or
//!   `*_tests.rs`, and files whose top level carries `#![cfg(test)]`;
//! - every item gated by `#[cfg(test)]` or a `cfg(all(test, ..))` predicate
//!   (module, fn, impl, const, static, use, ...), however its attributes and
//!   tokens are laid out;
//! - every item decorated by a test attribute macro (last path segment `test`,
//!   `rstest`, `test_case` or `bench`, e.g. `#[tokio::test(..)]`), wherever it
//!   lives;
//! - the file behind an external `mod name;` that sits in test code, resolved
//!   through the Rust module rules and `#[path]`, transitively.
//!
//! Braces are matched on a copy of the source with comments, string and char
//! literals blanked, so brace characters inside them do not shift a span.
//! Build scripts and non-test `src/` code are not test code.

// Rust guideline compliant 2026-10-01

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::XtaskError;

/// Directory below the workspace root whose `.rs` files [`read_sources`] loads.
const SOURCE_ROOT: &str = "crates";

/// Directory name that [`read_sources`] never descends into (build output).
const BUILD_OUTPUT_DIR: &str = "target";

/// Source text in two aligned views.
///
/// Both views have the byte length of the original text and keep every
/// newline, so an offset found in one view is the same offset in the other and
/// in the original.
#[derive(Debug)]
pub struct Views {
    /// Only comments blanked; string and char literals are intact, for pattern
    /// search that must read literal contents.
    pub code: String,
    /// Comments, string literals and char literals blanked, for structural
    /// scanning where text inside a literal must never match.
    pub skeleton: String,
}

/// One workspace source file together with its test-code classification.
///
/// Obtained from [`classify`]; offsets are byte offsets into the file's text
/// and valid in both [`Views`].
#[derive(Debug)]
pub struct SourceFile {
    path: String,
    views: Views,
    whole_file: bool,
    test_spans: Vec<TestSpan>,
    line_starts: Vec<usize>,
}

/// A test-only item (cfg-gated or test-attribute-decorated) of a file.
#[derive(Debug)]
struct TestSpan {
    start: usize,
    end: usize,
    /// Attribute bodies (text between `[` and `]`, comments blanked) of the item.
    attributes: Vec<String>,
}

impl SourceFile {
    /// Workspace-relative path of the file, with `/` separators.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The comment-blanked and literal-blanked views of the file text.
    #[must_use]
    pub fn views(&self) -> &Views {
        &self.views
    }

    /// Whether the entire file is test code (by path, a file-level
    /// `#![cfg(test)]`, or because a test module elsewhere declares it).
    #[must_use]
    pub fn is_test_file(&self) -> bool {
        self.whole_file
    }

    /// Whether the byte `offset` lies in test code: anywhere in a test file,
    /// or inside an attributed test item of an ordinary file.
    #[must_use]
    pub fn in_test_code(&self, offset: usize) -> bool {
        self.whole_file
            || self
                .test_spans
                .iter()
                .any(|span| span.start <= offset && offset < span.end)
    }

    /// Attribute bodies (text between `[` and `]`, comments blanked) of every
    /// test-only item that contains the byte `offset`, outermost first.
    ///
    /// A scan that treats some test items differently, such as those declared
    /// `#[tokio::test(start_paused = true)]`, reads the attribute text here.
    #[must_use]
    pub fn test_attributes_at(&self, offset: usize) -> Vec<&str> {
        self.test_spans
            .iter()
            .filter(|span| span.start <= offset && offset < span.end)
            .flat_map(|span| span.attributes.iter().map(String::as_str))
            .collect()
    }

    /// One-based line number of the byte `offset`.
    #[must_use]
    pub fn line_of(&self, offset: usize) -> usize {
        self.line_starts.partition_point(|&start| start <= offset)
    }
}

/// Whether `byte` can be part of an identifier.
#[must_use]
pub fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Index of the first non-whitespace byte at or after `from`.
#[must_use]
pub fn skip_whitespace(bytes: &[u8], mut from: usize) -> usize {
    while bytes.get(from).is_some_and(u8::is_ascii_whitespace) {
        from += 1;
    }
    from
}

/// Content of the string literal (plain or raw) that starts at `start` in
/// `code`, or `None` when no such literal starts there.
///
/// `code` must be a view in which string literals are intact, such as
/// [`Views::code`].
#[must_use]
pub fn string_literal_at(code: &str, start: usize) -> Option<&str> {
    let bytes = code.as_bytes();
    match bytes.get(start)? {
        b'"' => {
            let end = end_of_string(bytes, start);
            code.get(start + 1..end.checked_sub(1)?)
        }
        b'r' => {
            let end = end_of_raw_string(bytes, start)?;
            let hashes = bytes[start + 1..]
                .iter()
                .take_while(|&&b| b == b'#')
                .count();
            code.get(start + 2 + hashes..end.checked_sub(1 + hashes)?)
        }
        _ => None,
    }
}

/// Builds the [`Views`] of `text`.
#[must_use]
pub fn views(text: &str) -> Views {
    let bytes = text.as_bytes();
    let mut code = bytes.to_vec();
    let mut skeleton = bytes.to_vec();
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        let prev_is_ident = i > 0 && is_ident(bytes[i - 1]);
        if byte == b'/' && bytes.get(i + 1) == Some(&b'/') {
            let end = text[i..].find('\n').map_or(bytes.len(), |n| i + n);
            blank(&mut code, i, end);
            blank(&mut skeleton, i, end);
            i = end;
        } else if byte == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let mut depth = 0;
            let mut j = i;
            while j < bytes.len() {
                if bytes[j..].starts_with(b"/*") {
                    depth += 1;
                    j += 2;
                } else if bytes[j..].starts_with(b"*/") {
                    depth -= 1;
                    j += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    j += 1;
                }
            }
            blank(&mut code, i, j);
            blank(&mut skeleton, i, j);
            i = j;
        } else if byte == b'"' {
            let end = end_of_string(bytes, i);
            blank(&mut skeleton, i, end);
            i = end;
        } else if matches!(byte, b'r' | b'b' | b'c') && !prev_is_ident {
            // `r`, `br` and `cr` open a raw string; `b"`, `c"` and `b'` open
            // ordinary literals that the quote branches handle on the next byte.
            let raw_start = if byte == b'r' { i } else { i + 1 };
            let raw = (bytes.get(raw_start) == Some(&b'r'))
                .then(|| end_of_raw_string(bytes, raw_start))
                .flatten();
            if let Some(end) = raw {
                blank(&mut skeleton, i, end);
                i = end;
            } else {
                i += 1;
            }
        } else if byte == b'\'' {
            if let Some(end) = end_of_char(text, i) {
                blank(&mut skeleton, i, end);
                i = end;
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    Views {
        code: String::from_utf8(code).expect("blanking keeps UTF-8 valid"),
        skeleton: String::from_utf8(skeleton).expect("blanking keeps UTF-8 valid"),
    }
}

/// Classifies `files`, a map of workspace-relative path to source text.
///
/// A file is test code when its path says so or an external `mod` in test code
/// declares it; the result holds one [`SourceFile`] per input path. Modules are
/// resolved only against the paths in `files`, so a declared module whose file
/// is absent from the map is ignored.
#[must_use]
pub fn classify(files: &BTreeMap<String, String>) -> BTreeMap<String, SourceFile> {
    let parsed: BTreeMap<&str, Parsed> = files
        .iter()
        .map(|(path, text)| (path.as_str(), Parsed::new(path, text)))
        .collect();
    let mut forced: BTreeSet<&str> = BTreeSet::new();
    loop {
        let before = forced.len();
        for (&path, file) in &parsed {
            let whole_file = file.whole_file(forced.contains(path));
            let modules = external_modules(&file.views, &file.items, whole_file);
            for module in modules.iter().filter(|module| module.in_test_code) {
                let found = module_candidates(path, module)
                    .into_iter()
                    .find_map(|candidate| files.get_key_value(&candidate).map(|(k, _)| k));
                if let Some(found) = found {
                    forced.insert(found.as_str());
                }
            }
        }
        if forced.len() == before {
            break;
        }
    }
    files
        .iter()
        .zip(parsed)
        .map(|((path, text), (_, file))| {
            let whole_file = file.whole_file(forced.contains(path.as_str()));
            let test_spans = file
                .items
                .iter()
                .filter(|item| item.test_only)
                .map(|item| TestSpan {
                    start: item.start,
                    end: item.end,
                    attributes: item.attributes.clone(),
                })
                .collect();
            let line_starts = std::iter::once(0)
                .chain(text.match_indices('\n').map(|(at, _)| at + 1))
                .collect();
            let source = SourceFile {
                path: path.clone(),
                views: file.views,
                whole_file,
                test_spans,
                line_starts,
            };
            (path.clone(), source)
        })
        .collect()
}

/// Reads every `.rs` file below `crates/` under the workspace `root`,
/// skipping `target` directories.
///
/// Keys are workspace-relative paths with `/` separators, in sorted order.
///
/// # Errors
///
/// Returns [`XtaskError::Io`] when a directory or file cannot be read and
/// [`XtaskError::InvalidPath`] when a found file is not below `root`.
pub fn read_sources(root: &Path) -> Result<BTreeMap<String, String>, XtaskError> {
    let mut paths = Vec::new();
    collect_rust_files(&root.join(SOURCE_ROOT), &mut paths)?;
    let mut sources = BTreeMap::new();
    for file in paths {
        let relative = file
            .strip_prefix(root)
            .ok()
            .ok_or_else(|| XtaskError::InvalidPath(file.clone()))?
            .to_string_lossy()
            .replace('\\', "/");
        let text = fs::read_to_string(&file).map_err(|source| XtaskError::Io {
            path: file.clone(),
            source,
        })?;
        sources.insert(relative, text);
    }
    Ok(sources)
}

/// Collects every `.rs` file below `dir`, skipping build output.
fn collect_rust_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), XtaskError> {
    for entry in crate::read_dir(dir)? {
        let path = entry.path();
        if path.is_dir() {
            if path
                .file_name()
                .is_some_and(|name| name == BUILD_OUTPUT_DIR)
            {
                continue;
            }
            collect_rust_files(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// Whether the whole file is test code judged by its path alone.
fn path_is_test(path: &str) -> bool {
    let mut parts: Vec<&str> = path.split('/').collect();
    let Some(file) = parts.pop() else {
        return false;
    };
    parts.contains(&"tests") || file == "tests.rs" || file.ends_with("_tests.rs")
}

/// Overwrites `bytes[from..to]` with spaces, keeping newlines so offsets and
/// line numbers stay aligned with the original text.
fn blank(bytes: &mut [u8], from: usize, to: usize) {
    for byte in &mut bytes[from..to] {
        if *byte != b'\n' {
            *byte = b' ';
        }
    }
}

/// Index one past the closing quote of the string literal whose opening quote
/// is at `start`.
fn end_of_string(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

/// Index one past a raw string literal whose `r` is at `start`, or
/// `None` when `start` does not begin one.
fn end_of_raw_string(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    let mut hashes = 0;
    while bytes.get(i) == Some(&b'#') {
        hashes += 1;
        i += 1;
    }
    if bytes.get(i) != Some(&b'"') {
        return None;
    }
    i += 1;
    while i < bytes.len() {
        if bytes[i] == b'"' && bytes[i + 1..].iter().take_while(|&&b| b == b'#').count() >= hashes {
            return Some(i + 1 + hashes);
        }
        i += 1;
    }
    Some(bytes.len())
}

/// Index one past a char literal starting at `start`, or `None` for a lifetime.
fn end_of_char(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.get(start + 1) == Some(&b'\\') {
        let close = text.get(start + 3..)?.find('\'')?;
        return Some(start + 3 + close + 1);
    }
    let ch = text.get(start + 1..)?.chars().next()?;
    let close = start + 1 + ch.len_utf8();
    (bytes.get(close) == Some(&b'\'')).then_some(close + 1)
}

/// Index of the byte matching the opener at `open`, or the text length.
fn matching_close(bytes: &[u8], open: usize) -> usize {
    let (opener, closer) = match bytes[open] {
        b'[' => (b'[', b']'),
        b'(' => (b'(', b')'),
        _ => (b'{', b'}'),
    };
    let mut depth = 0;
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

/// Index one past the item that starts at `from`: the first `;` outside
/// parentheses and brackets, the brace block that follows a header, or the
/// unmatched closer of the enclosing scope.
fn end_of_item(bytes: &[u8], from: usize) -> usize {
    let mut depth = 0_usize;
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b'(' | b'[' => depth += 1,
            b')' | b']' => {
                if depth == 0 {
                    return i;
                }
                depth -= 1;
            }
            b';' if depth == 0 => return i + 1,
            b'{' if depth == 0 => return matching_close(bytes, i) + 1,
            b'}' if depth == 0 => return i,
            _ => {}
        }
        i += 1;
    }
    bytes.len()
}

/// Whether a `cfg` predicate is satisfied only when `cfg(test)` is on:
/// `test`, or `all(..)` with such an argument. `any` and `not` are not.
fn is_test_only(predicate: &str) -> bool {
    let predicate = predicate.trim();
    if predicate == "test" {
        return true;
    }
    let Some(arguments) = predicate
        .strip_prefix("all")
        .map(str::trim_start)
        .and_then(|rest| rest.strip_prefix('('))
        .and_then(|rest| rest.strip_suffix(')'))
    else {
        return false;
    };
    let mut depth = 0_usize;
    let mut start = 0;
    let mut parts = Vec::new();
    for (i, c) in arguments.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&arguments[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&arguments[start..]);
    parts.into_iter().any(is_test_only)
}

/// Whether the attribute body (text between `[` and `]`) is a test-only `cfg`.
fn is_test_cfg(attribute: &str) -> bool {
    attribute
        .trim()
        .strip_prefix("cfg")
        .map(str::trim_start)
        .and_then(|rest| rest.strip_prefix('('))
        .and_then(|rest| rest.trim_end().strip_suffix(')'))
        .is_some_and(is_test_only)
}

/// Attribute macros, by last path segment, that mark the item they decorate as
/// a test or benchmark.
const TEST_ATTRIBUTE_NAMES: [&str; 4] = ["test", "rstest", "test_case", "bench"];

/// Whether the attribute body (text between `[` and `]`) names a test
/// attribute macro such as `test`, `tokio::test(flavor = "..")`, `rstest`,
/// `test_case(..)` or `bench`.
fn is_test_marker(attribute: &str) -> bool {
    let end = attribute
        .find(['(', '=', '[', '{'])
        .unwrap_or(attribute.len());
    let path: String = attribute[..end]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    path.rsplit("::")
        .next()
        .is_some_and(|last| TEST_ATTRIBUTE_NAMES.contains(&last))
}

/// Extracts the string from a `path = "..."` attribute body read from the
/// comment-free view.
fn path_attribute(code_body: &str) -> Option<String> {
    let rest = code_body.trim().strip_prefix("path")?.trim_start();
    let rest = rest.strip_prefix('=')?.trim();
    Some(rest.strip_prefix('"')?.strip_suffix('"')?.to_owned())
}

/// A `mod name;` declaration found in the file.
struct ExternalModule {
    name: String,
    /// Names of the inline modules that enclose the declaration.
    inline_chain: Vec<String>,
    path_attribute: Option<String>,
    in_test_code: bool,
}

/// An attribute run and the item it decorates.
struct AttributedItem {
    start: usize,
    /// Offset of the first token after the attributes.
    header_start: usize,
    end: usize,
    test_only: bool,
    path_attribute: Option<String>,
    /// Attribute bodies in source order, read from the comment-free view.
    attributes: Vec<String>,
}

/// The per-file lexing and attribute analysis that does not depend on which
/// other files are test modules.
struct Parsed {
    views: Views,
    items: Vec<AttributedItem>,
    /// Whether the path or a file-level `#![cfg(test)]` marks the whole file.
    self_test: bool,
}

impl Parsed {
    fn new(path: &str, text: &str) -> Self {
        let views = views(text);
        let (items, inner_cfg_test) = attributed_items(&views);
        Self {
            views,
            items,
            self_test: inner_cfg_test || path_is_test(path),
        }
    }

    /// Whether the whole file is test code; `declared_by_test` is set when an
    /// external `mod` in test code names this file.
    fn whole_file(&self, declared_by_test: bool) -> bool {
        self.self_test || declared_by_test
    }
}

/// Finds every attribute run (`#[..]` sequences) with the item it decorates,
/// and whether a file-level `#![cfg(test)]` marks the whole file.
fn attributed_items(views: &Views) -> (Vec<AttributedItem>, bool) {
    let skeleton = views.skeleton.as_bytes();
    let code = views.code.as_bytes();
    let mut items = Vec::new();
    let mut file_is_test = false;
    let mut brace_depth = 0_usize;
    let mut i = 0;
    while i < skeleton.len() {
        match skeleton[i] {
            b'{' => brace_depth += 1,
            b'}' => brace_depth = brace_depth.saturating_sub(1),
            b'#' => {
                let inner = skeleton.get(i + 1) == Some(&b'!');
                let open = i + 1 + usize::from(inner);
                if skeleton.get(open) == Some(&b'[') {
                    if inner {
                        let close = matching_close(skeleton, open);
                        if brace_depth == 0 && is_test_cfg(&views.skeleton[open + 1..close]) {
                            file_is_test = true;
                        }
                        i = close;
                    } else {
                        let start = i;
                        let mut cursor = i;
                        let mut test_only = false;
                        let mut path = None;
                        let mut attributes = Vec::new();
                        while skeleton.get(cursor) == Some(&b'#')
                            && skeleton.get(cursor + 1) == Some(&b'[')
                        {
                            let close = matching_close(skeleton, cursor + 1);
                            let body = &views.skeleton[cursor + 2..close];
                            test_only |= is_test_cfg(body) || is_test_marker(body);
                            let code_body = String::from_utf8_lossy(&code[cursor + 2..close]);
                            if path.is_none() {
                                path = path_attribute(&code_body);
                            }
                            attributes.push(code_body.into_owned());
                            cursor = close + 1;
                            while skeleton.get(cursor).is_some_and(u8::is_ascii_whitespace) {
                                cursor += 1;
                            }
                        }
                        items.push(AttributedItem {
                            start,
                            header_start: cursor,
                            end: end_of_item(skeleton, cursor),
                            test_only,
                            path_attribute: path,
                            attributes,
                        });
                        i = cursor.max(i + 1) - 1;
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    (items, file_is_test)
}

/// Parses the word after `mod` and what follows it, starting at the `mod`
/// keyword at `at`. Returns the name and the index of `{` or `;`.
fn module_header(skeleton: &[u8], at: usize) -> Option<(String, usize, u8)> {
    let mut i = at + 3;
    if !skeleton.get(i).is_some_and(u8::is_ascii_whitespace) {
        return None;
    }
    while skeleton.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    let name_start = i;
    while skeleton.get(i).copied().is_some_and(is_ident) {
        i += 1;
    }
    if i == name_start {
        return None;
    }
    let name = String::from_utf8_lossy(&skeleton[name_start..i]).into_owned();
    while skeleton.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    match skeleton.get(i) {
        Some(&c) if c == b'{' || c == b';' => Some((name, i, c)),
        _ => None,
    }
}

/// Lists the `mod name;` declarations and marks which sit in test code.
fn external_modules(
    views: &Views,
    items: &[AttributedItem],
    whole_file: bool,
) -> Vec<ExternalModule> {
    let skeleton = views.skeleton.as_bytes();
    let in_test = |position: usize| {
        whole_file
            || items
                .iter()
                .any(|item| item.test_only && item.start <= position && position < item.end)
    };
    let mut inline: Vec<(String, usize, usize)> = Vec::new();
    let mut external = Vec::new();
    let mut i = 0;
    while i + 3 < skeleton.len() {
        let is_keyword = &skeleton[i..i + 3] == b"mod" && (i == 0 || !is_ident(skeleton[i - 1]));
        if let Some((name, at, kind)) = is_keyword.then(|| module_header(skeleton, i)).flatten() {
            if kind == b'{' {
                inline.push((name, at, matching_close(skeleton, at)));
            } else {
                let path = items
                    .iter()
                    .find(|item| {
                        item.header_start <= i
                            && views.skeleton[item.header_start..i]
                                .trim()
                                .strip_prefix("pub")
                                .map_or_else(
                                    || views.skeleton[item.header_start..i].trim().is_empty(),
                                    |rest| !rest.contains(['{', ';']),
                                )
                    })
                    .and_then(|item| item.path_attribute.clone());
                external.push((name, i, path));
            }
            i = at;
        }
        i += 1;
    }
    external
        .into_iter()
        .map(|(name, position, path_attribute)| ExternalModule {
            name,
            inline_chain: inline
                .iter()
                .filter(|(_, open, close)| *open < position && position < *close)
                .map(|(module, _, _)| module.clone())
                .collect(),
            path_attribute,
            in_test_code: in_test(position),
        })
        .collect()
}

/// Joins `parts` with `/` and resolves `.` and `..` components.
fn normalize(parts: &[&str]) -> String {
    let mut out: Vec<&str> = Vec::new();
    for component in parts.iter().flat_map(|part| part.split('/')) {
        match component {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out.join("/")
}

/// Candidate files for `module`, declared in the file at `declaring`.
fn module_candidates(declaring: &str, module: &ExternalModule) -> Vec<String> {
    let (parent, file) = declaring.rsplit_once('/').unwrap_or(("", declaring));
    let stem = file.strip_suffix(".rs").unwrap_or(file);
    let chain = module.inline_chain.join("/");
    if let Some(path) = &module.path_attribute {
        return vec![normalize(&[parent, &chain, path])];
    }
    let mut bases = Vec::new();
    if !matches!(stem, "mod" | "lib" | "main") {
        bases.push(normalize(&[parent, stem]));
    }
    bases.push(normalize(&[parent]));
    bases
        .into_iter()
        .flat_map(|base| {
            [
                normalize(&[&base, &chain, &format!("{}.rs", module.name)]),
                normalize(&[&base, &chain, &module.name, "mod.rs"]),
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{classify, read_sources, views, SourceFile};
    use std::collections::BTreeMap;
    use std::fs;

    /// Token the fixtures plant (as `@`) to probe whether a spot is test code.
    const PROBE: &str = "PROBE";

    /// Fixture source with `@` standing for the probe token.
    fn source(template: &str) -> String {
        template.replace('@', PROBE)
    }

    fn tree(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(path, template)| ((*path).to_owned(), source(template)))
            .collect()
    }

    /// Lines of the probe tokens that are code (not comment or literal text) in
    /// test code of `file`.
    fn probe_lines(file: &SourceFile) -> Vec<usize> {
        file.views()
            .skeleton
            .match_indices(PROBE)
            .filter(|&(offset, _)| file.in_test_code(offset))
            .map(|(offset, _)| file.line_of(offset))
            .collect()
    }

    /// Probe lines found in test code of the single file `path`.
    fn lines_of(path: &str, template: &str) -> Vec<usize> {
        let classified = classify(&tree(&[(path, template)]));
        probe_lines(&classified[path])
    }

    /// Paths of the files that hold a probe in test code.
    fn paths_with_probes(files: &BTreeMap<String, String>) -> Vec<String> {
        classify(files)
            .into_iter()
            .filter(|(_, file)| !probe_lines(file).is_empty())
            .map(|(path, _)| path)
            .collect()
    }

    #[test]
    fn flags_a_cfg_test_fn() {
        let template = "#[cfg(test)]\nfn helper() -> String {\n    let p = @;\n    p\n}\n\nfn tool() {\n    let p = @;\n}\n";
        assert_eq!(lines_of("crates/demo/src/lib.rs", template), [3]);
    }

    #[test]
    fn flags_a_cfg_test_impl_const_static_and_use() {
        let template = "struct S;\n#[cfg(test)]\nimpl S {\n    fn f() {\n        let p = @;\n    }\n}\n#[cfg(test)]\nconst A: &str = @;\n#[cfg(test)]\nstatic B: [&str; 1] = [@];\n#[cfg(test)]\nuse std::path::Path;\nfn tool() {\n    let p = @;\n}\n";
        assert_eq!(lines_of("crates/demo/src/lib.rs", template), [5, 9, 11]);
    }

    #[test]
    fn flags_an_attribute_and_item_on_one_line() {
        let template = "#[cfg(test)] fn helper() { let p = @; }\nfn tool() { let p = @; }\n";
        assert_eq!(lines_of("crates/demo/src/lib.rs", template), [1]);
    }

    #[test]
    fn flags_a_cfg_test_item_with_extra_whitespace_and_other_attributes() {
        let template = "#[ cfg ( test ) ]\n\n#[allow(dead_code)]\n#[inline]\nfn helper() {\n    let p = @;\n}\n#[allow(dead_code)]\n#[cfg(test)]\n#[inline]\nfn second() {\n    let p = @;\n}\n";
        assert_eq!(lines_of("crates/demo/src/lib.rs", template), [6, 12]);
    }

    #[test]
    fn flags_items_marked_by_a_test_attribute_macro_in_ordinary_source() {
        let forms = [
            "#[test]\nfn f() {\n    let p = @;\n}\n",
            "#[tokio::test(flavor = \"multi_thread\")]\nasync fn f() {\n    let p = @;\n}\n",
            "#[tokio::test]\nasync fn f() {\n    let p = @;\n}\n",
            "#[rstest]\nfn f() {\n    let p = @;\n}\n",
            "#[rstest::rstest]\n#[case(1)]\nfn f(n: u8) {\n    let p = @;\n}\n",
            "#[test_case(1, 2)]\n#[test_case(3, 4)]\nfn f(a: u8, b: u8) {\n    let p = @;\n}\n",
            "#[bench]\nfn f(b: &mut Bencher) {\n    let p = @;\n}\n",
            "#[test]\n#[ignore]\n#[should_panic(expected = \"x\")]\nfn f() {\n    let p = @;\n}\n",
            "#[ignore]\n/* note */ #[ test ] // trailing\n#[should_panic]\nfn f() {\n    let p = @;\n}\n",
            "#[ std :: prelude :: v1 :: test ] fn f() { let p = @; }\n",
        ];
        for form in forms {
            let text = format!("{form}fn tool() {{\n    let p = @;\n}}\n");
            let lines = lines_of("crates/demo/src/lib.rs", &text);
            assert_eq!(lines.len(), 1, "{form}: {lines:?}");
            assert!(
                lines[0] < text.matches('\n').count() - 1,
                "{form}: {lines:?}"
            );
        }
    }

    #[test]
    fn ignores_attributes_that_only_resemble_test_markers() {
        let forms = [
            "#[derive(Test)]\nstruct S;\nfn f() {\n    let p = @;\n}\n",
            "#[attr_test]\nfn f() {\n    let p = @;\n}\n",
            "#[test_helper]\nfn f() {\n    let p = @;\n}\n",
            "#[tests]\nfn f() {\n    let p = @;\n}\n",
            "#[doc = \"test\"]\nfn f() {\n    let p = @;\n}\n",
            "#[cfg(feature = \"test\")]\nfn f() {\n    let p = @;\n}\n",
            "#[allow(test)]\nfn f() {\n    let p = @;\n}\n",
        ];
        for form in forms {
            assert!(
                lines_of("crates/demo/src/lib.rs", form).is_empty(),
                "{form}"
            );
        }
    }

    #[test]
    fn flags_cfg_all_test_gates_but_not_any_or_not() {
        let template = "#[cfg(all(test, unix))]\nfn a() { let p = @; }\n#[cfg(all(unix, feature = \"x\", test))]\nfn b() { let p = @; }\n#[cfg(any(test, unix))]\nfn c() { let p = @; }\n#[cfg(not(test))]\nfn d() { let p = @; }\n#[cfg(unix)]\nfn e() { let p = @; }\n";
        assert_eq!(lines_of("crates/demo/src/lib.rs", template), [2, 4]);
    }

    #[test]
    fn flags_nested_items_and_survives_braces_in_literals() {
        let template = "#[cfg(test)]\nmod tests {\n    fn braces() {\n        let a = \"}}}\";\n        let b = '}';\n        let c = r#\"{ \"}\"#;\n        // }\n        /* } */\n    }\n    mod inner {\n        fn deep() {\n            let p = @;\n        }\n    }\n    fn tail() {\n        let p = @;\n    }\n}\nfn tool() {\n    let p = @;\n}\n";
        assert_eq!(lines_of("crates/demo/src/lib.rs", template), [12, 16]);
    }

    #[test]
    fn a_file_level_cfg_test_attribute_marks_the_whole_file() {
        let template = "#![cfg(test)]\nfn helper() {\n    let p = @;\n}\n";
        assert_eq!(lines_of("crates/demo/src/support.rs", template), [3]);
    }

    #[test]
    fn a_raw_byte_or_c_string_with_quotes_does_not_hide_a_later_lookup() {
        let literals = [
            r##"let s = br#"a"{"#;"##,
            r###"let s = br##"a"#{"##;"###,
            r##"let s = cr#"a"{"#;"##,
            r#"let s = br"a{";"#,
            r#"let s = cr"a{";"#,
            r#"let s = b"a\"{";"#,
            r#"let s = c"a\"{";"#,
            r##"let s = r#"a"{"#;"##,
        ];
        for literal in literals {
            let template =
                format!("#[cfg(test)]\nmod tests {{\n    fn f() {{ {literal} }}\n    fn g() {{ let p = @; }}\n}}\nfn tool() {{ let p = @; }}\n");
            assert_eq!(
                lines_of("crates/demo/src/lib.rs", &template),
                [4],
                "{literal}"
            );
            let template = format!(
                "fn tool() {{ let p = @; }}\n#[test]\nfn f() {{\n    {literal}\n    let p = @;\n}}\n"
            );
            assert_eq!(
                lines_of("crates/demo/src/lib.rs", &template),
                [5],
                "{literal}"
            );
        }
    }

    #[test]
    fn identifiers_ending_in_literal_prefixes_do_not_open_raw_strings() {
        let template = "fn f() {\n    let bar = 1;\n    let c = bar;\n    let p = @;\n}\n#[cfg(test)]\nmod tests {\n    fn g() { let for_c = r#type; let p = @; }\n}\n";
        assert_eq!(lines_of("crates/demo/src/lib.rs", template), [8]);
        let template =
            "#[test]\nfn f() {\n    let aBr = 1;\n    let bbr = aBr;\n    let p = @;\n}\n";
        assert_eq!(lines_of("crates/demo/src/lib.rs", template), [5]);
    }

    #[test]
    fn lexer_handles_nested_comments_lifetimes_chars_and_escapes() {
        let cases: [(&str, &str, Vec<usize>); 8] = [
            (
                "nested block comments",
                "#[test]\nfn f() {\n    /* a /* } */ } */\n    let p = @;\n}\nfn tool() { let p = @; }\n",
                vec![4],
            ),
            (
                "lifetimes and brace chars",
                "#[test]\nfn f<'a>(x: &'a str) {\n    let a = '}';\n    let b = '{';\n    let c = '\"';\n    let p = @;\n}\nfn tool<'a>(x: &'a str) { let p = @; }\n",
                vec![6],
            ),
            (
                "escaped quote and backslash",
                "#[test]\nfn f() {\n    let a = \"q\\\" }\";\n    let b = \"back\\\\\";\n    let c = '\\'';\n    let d = '\\\\';\n    let p = @;\n}\nfn tool() { let p = @; }\n",
                vec![7],
            ),
            (
                "comment markers in strings",
                "#[test]\nfn f() {\n    let a = \"// }\";\n    let b = \"/* }\";\n    let c = r#\"// } /*\"#;\n    let p = @;\n}\nfn tool() { let p = @; }\n",
                vec![6],
            ),
            (
                "doc comments with a lookup",
                "/// let p = @;\n//! let p = @;\n/** let p = @; */\n/*! let p = @; */\n#[test]\nfn f() {}\n",
                vec![],
            ),
            (
                "byte char braces",
                "#[test]\nfn f() {\n    let a = b'}';\n    let b = b'\\'';\n    let p = @;\n}\nfn tool() { let p = @; }\n",
                vec![5],
            ),
            (
                "raw string with hashes and comment markers",
                "#[test]\nfn f() {\n    let a = r##\"\"# } // /*\"##;\n    let p = @;\n}\nfn tool() { let p = @; }\n",
                vec![4],
            ),
            (
                "unicode char and string",
                "#[test]\nfn f() {\n    let a = 'é';\n    let b = \"é}\";\n    let p = @;\n}\nfn tool() { let p = @; }\n",
                vec![5],
            ),
        ];
        for (name, template, expected) in cases {
            assert_eq!(
                lines_of("crates/demo/src/lib.rs", template),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn external_module_resolution_follows_mod_rs_nested_files_and_path_attributes() {
        let files = tree(&[
            (
                "crates/demo/src/lib.rs",
                "#[cfg(test)] mod a;\n#[cfg(test)]\n#[path = \"support/custom.rs\"]\nmod b;\n#[cfg(test)]\nmod c {\n    mod d;\n}\n",
            ),
            ("crates/demo/src/a/mod.rs", "mod deep;\n"),
            ("crates/demo/src/a/deep.rs", "fn f() { let p = @; }\n"),
            ("crates/demo/src/support/custom.rs", "fn f() { let p = @; }\n"),
            ("crates/demo/src/c/d.rs", "fn f() { let p = @; }\n"),
            ("crates/demo/src/other.rs", "fn f() { let p = @; }\n"),
        ]);
        assert_eq!(
            paths_with_probes(&files),
            [
                "crates/demo/src/a/deep.rs",
                "crates/demo/src/c/d.rs",
                "crates/demo/src/support/custom.rs",
            ]
        );
    }

    #[test]
    fn test_named_files_are_test_code_and_other_source_is_not() {
        for path in [
            "crates/demo/tests/run.rs",
            "crates/demo/src/tests.rs",
            "crates/demo/src/governance/owner_tests.rs",
            "crates/demo/src/session/tests/fixtures.rs",
        ] {
            assert_eq!(lines_of(path, "let p = @;\n"), [1], "{path}");
        }
        for path in ["crates/demo/src/lib.rs", "crates/demo/build.rs"] {
            assert!(lines_of(path, "let p = @;\n").is_empty(), "{path}");
        }
    }

    #[test]
    fn a_module_declared_by_a_test_module_is_a_test_file_but_its_siblings_are_not() {
        let files = tree(&[
            (
                "crates/demo/src/lib.rs",
                "#[cfg(test)]\nmod helpers;\nmod plain;\n",
            ),
            ("crates/demo/src/helpers.rs", "fn h() {}\n"),
            ("crates/demo/src/plain.rs", "fn p() {}\n"),
        ]);
        let classified = classify(&files);
        assert!(classified["crates/demo/src/helpers.rs"].is_test_file());
        assert!(!classified["crates/demo/src/plain.rs"].is_test_file());
        assert!(!classified["crates/demo/src/lib.rs"].is_test_file());
    }

    #[test]
    fn a_declared_module_missing_from_the_input_is_ignored() {
        let files = tree(&[("crates/demo/src/lib.rs", "#[cfg(test)]\nmod absent;\n")]);
        assert_eq!(classify(&files).len(), 1);
    }

    #[test]
    fn offsets_map_to_one_based_lines_and_test_spans() {
        let text = "fn a() {}\n#[test]\nfn b() {\n    x\n}\nfn c() {}\n";
        let files = BTreeMap::from([("crates/demo/src/lib.rs".to_owned(), text.to_owned())]);
        let classified = classify(&files);
        let file = &classified["crates/demo/src/lib.rs"];
        assert_eq!(file.path(), "crates/demo/src/lib.rs");
        assert_eq!(file.line_of(0), 1);
        assert_eq!(file.line_of(text.find('\n').expect("newline")), 1);
        assert_eq!(file.line_of(text.find("#[test]").expect("marker")), 2);
        assert_eq!(file.line_of(text.find('x').expect("body")), 4);
        assert_eq!(file.line_of(text.find("fn c").expect("tail")), 6);
        assert!(!file.is_test_file());
        assert!(!file.in_test_code(text.find("fn a").expect("head")));
        assert!(file.in_test_code(text.find('x').expect("body")));
        assert!(!file.in_test_code(text.find("fn c").expect("tail")));
    }

    #[test]
    fn test_attributes_at_lists_the_attributes_of_enclosing_test_items() {
        let text = "fn plain() { a }
#[cfg(test)]
mod tests {
    #[tokio::test(start_paused = true)]
    #[ignore]
    async fn paused() { b }
    #[test]
    fn other() { c }
}
";
        let files = BTreeMap::from([("crates/demo/src/lib.rs".to_owned(), text.to_owned())]);
        let classified = classify(&files);
        let file = &classified["crates/demo/src/lib.rs"];
        let at = |needle: &str| file.test_attributes_at(text.find(needle).expect("needle"));
        assert!(at(" a }").is_empty());
        assert_eq!(
            at(" b }"),
            ["cfg(test)", "tokio::test(start_paused = true)", "ignore"]
        );
        assert_eq!(at(" c }"), ["cfg(test)", "test"]);
    }

    #[test]
    fn views_blank_comments_and_literals_without_moving_offsets() {
        let text = "let a = \"s}\"; // note\nlet b = 'c'; /* x\ny */ let c = 1;\n";
        let blanked = views(text);
        assert_eq!(blanked.code.len(), text.len());
        assert_eq!(blanked.skeleton.len(), text.len());
        assert!(
            blanked.code.contains("\"s}\""),
            "literal stays in code view"
        );
        assert!(!blanked.code.contains("note"));
        assert!(!blanked.skeleton.contains("s}"));
        assert!(!blanked.skeleton.contains("'c'"));
        assert!(blanked.skeleton.contains("let c = 1;"));
        assert_eq!(
            blanked.code.matches('\n').count(),
            text.matches('\n').count()
        );
    }

    #[test]
    fn read_sources_loads_rust_files_below_crates_and_skips_build_output() {
        let root = pohunek_test_support::tempdir().expect("temporary workspace");
        let write = |relative: &str, body: &str| {
            let path = root.path().join(relative);
            fs::create_dir_all(path.parent().expect("parent")).expect("create directory");
            fs::write(path, body).expect("write file");
        };
        write("crates/demo/src/lib.rs", "pub fn a() {}\n");
        write("crates/demo/tests/run.rs", "fn b() {}\n");
        write("crates/demo/target/debug/build.rs", "fn built() {}\n");
        write("crates/demo/README.md", "not rust\n");
        write("outside/src/lib.rs", "fn outside() {}\n");
        let sources = read_sources(root.path()).expect("sources load");
        assert_eq!(
            sources.keys().map(String::as_str).collect::<Vec<_>>(),
            ["crates/demo/src/lib.rs", "crates/demo/tests/run.rs"]
        );
        assert_eq!(sources["crates/demo/tests/run.rs"], "fn b() {}\n");
    }

    #[test]
    fn read_sources_reports_a_missing_crates_directory() {
        let root = pohunek_test_support::tempdir().expect("temporary workspace");
        let error = read_sources(root.path()).expect_err("no crates directory");
        assert!(matches!(error, crate::XtaskError::Io { .. }), "{error}");
    }
}
