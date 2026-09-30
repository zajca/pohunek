//! Keeps compile-time artifact lookups out of test code.
//!
//! A test that bakes `CARGO_BIN_EXE_*` or `CARGO_MANIFEST_DIR` into the binary
//! at compile time breaks when a nextest archive is extracted at another
//! absolute path. Test code resolves binaries, source files and the session
//! worker at run time through `pohunek_test_support` instead.
//!
//! Scanned as test code:
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
//! Build scripts and non-test `src/` code are not scanned; the tool's own
//! workspace lookup in `crates/xtask/src/lib.rs` is such code.

// Rust guideline compliant 2026-09-30

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Compile-time environment macros that bake a build-machine path into a test.
const LOOKUP_MACROS: [&str; 2] = ["env", "option_env"];

/// Variable that names the package directory at compile time.
const MANIFEST_DIR_VAR: &str = "CARGO_MANIFEST_DIR";

/// Prefix of the variables that name a package binary at compile time.
const BIN_EXE_PREFIX: &str = "CARGO_BIN_EXE_";

/// Files allowed to contain a forbidden pattern, as `(workspace-relative path,
/// reason)`. No file needs an exemption today; an entry must state why the
/// lookup cannot happen at run time.
const ALLOW_LIST: &[(&str, &str)] = &[];

/// One forbidden lookup found in test code.
#[derive(Debug, PartialEq, Eq)]
struct Violation {
    path: String,
    line: usize,
    /// The macro and variable that were read, as written in the source.
    lookup: String,
}

/// Whether the whole file is test code judged by its path alone.
fn is_test_file(path: &str) -> bool {
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

/// Whether `byte` can be part of an identifier.
fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
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

/// Source text in two aligned views: `code` has only comments blanked (string
/// literals intact, for pattern search); `skeleton` also has string and char
/// literals blanked (for structural scanning).
struct Views {
    code: String,
    skeleton: String,
}

fn views(text: &str) -> Views {
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

/// Result of scanning one file.
struct Analysis {
    violations: Vec<Violation>,
    /// Candidate files for every external module declared in test code, in
    /// resolution order; the first that exists is the module's file.
    test_module_candidates: Vec<Vec<String>>,
}

/// An attribute run and the item it decorates.
struct AttributedItem {
    start: usize,
    /// Offset of the first token after the attributes.
    header_start: usize,
    end: usize,
    test_only: bool,
    path_attribute: Option<String>,
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
                        while skeleton.get(cursor) == Some(&b'#')
                            && skeleton.get(cursor + 1) == Some(&b'[')
                        {
                            let close = matching_close(skeleton, cursor + 1);
                            let body = &views.skeleton[cursor + 2..close];
                            test_only |= is_test_cfg(body) || is_test_marker(body);
                            if path.is_none() {
                                let body = String::from_utf8_lossy(&code[cursor + 2..close]);
                                path = path_attribute(&body);
                            }
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

/// Index of the first non-whitespace byte at or after `from`.
fn skip_whitespace(bytes: &[u8], mut from: usize) -> usize {
    while bytes.get(from).is_some_and(u8::is_ascii_whitespace) {
        from += 1;
    }
    from
}

/// Content of the string literal (plain or raw) starting at `start`.
fn string_literal_at(code: &str, start: usize) -> Option<&str> {
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

/// Finds `env!` and `option_env!` invocations, with any path prefix and any
/// whitespace or comments between tokens, whose first argument is a string
/// literal naming `CARGO_MANIFEST_DIR` or a `CARGO_BIN_EXE_*` variable. Macro
/// names are located in the skeleton so text inside string literals is never
/// taken for an invocation. Returns the offset of the macro name and a
/// description of the lookup.
fn forbidden_lookups(views: &Views) -> Vec<(usize, String)> {
    let skeleton = views.skeleton.as_bytes();
    let mut found = Vec::new();
    for name in LOOKUP_MACROS {
        for (offset, _) in views.skeleton.match_indices(name) {
            let end = offset + name.len();
            if offset > 0 && is_ident(skeleton[offset - 1]) {
                continue;
            }
            let bang = skip_whitespace(skeleton, end);
            if skeleton.get(bang) != Some(&b'!') {
                continue;
            }
            let open = skip_whitespace(skeleton, bang + 1);
            if !matches!(skeleton.get(open), Some(b'(' | b'[' | b'{')) {
                continue;
            }
            // The skeleton blanks string literals, so the argument is located in the
            // comment-free view.
            let argument = skip_whitespace(views.code.as_bytes(), open + 1);
            let Some(variable) = string_literal_at(&views.code, argument) else {
                continue;
            };
            if variable == MANIFEST_DIR_VAR || variable.starts_with(BIN_EXE_PREFIX) {
                found.push((offset, format!("{name}!({variable:?}..)")));
            }
        }
    }
    found
}

/// Scans one file. `forced_test` marks the whole file as test code (declared
/// by a test module elsewhere).
fn analyze(path: &str, text: &str, forced_test: bool) -> Analysis {
    let views = views(text);
    let (items, inner_cfg_test) = attributed_items(&views);
    let whole_file = forced_test || inner_cfg_test || is_test_file(path);
    let test_spans: Vec<(usize, usize)> = items
        .iter()
        .filter(|item| item.test_only)
        .map(|item| (item.start, item.end))
        .collect();
    let mut violations = Vec::new();
    for (offset, lookup) in forbidden_lookups(&views) {
        let in_test = whole_file || test_spans.iter().any(|&(s, e)| s <= offset && offset < e);
        if in_test {
            violations.push(Violation {
                path: path.to_owned(),
                line: text[..offset].matches('\n').count() + 1,
                lookup,
            });
        }
    }
    violations.sort_by_key(|violation| violation.line);
    let test_module_candidates = external_modules(&views, &items, whole_file)
        .iter()
        .filter(|module| module.in_test_code)
        .map(|module| module_candidates(path, module))
        .collect();
    Analysis {
        violations,
        test_module_candidates,
    }
}

/// Scans `files` (workspace-relative path to text): a file is test code when
/// its path says so or an external `mod` in test code declares it.
fn scan_files(files: &BTreeMap<String, String>, allow: &[(&str, &str)]) -> Vec<Violation> {
    let mut forced: BTreeSet<String> = BTreeSet::new();
    loop {
        let before = forced.len();
        for (path, text) in files {
            let analysis = analyze(path, text, forced.contains(path));
            for candidates in analysis.test_module_candidates {
                if let Some(found) = candidates.into_iter().find(|c| files.contains_key(c)) {
                    forced.insert(found);
                }
            }
        }
        if forced.len() == before {
            break;
        }
    }
    files
        .iter()
        .filter(|(path, _)| !allow.iter().any(|(allowed, _)| allowed == path))
        .flat_map(|(path, text)| analyze(path, text, forced.contains(path)).violations)
        .collect()
}

/// Collects every `.rs` file below `dir`, skipping build output.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// Scans `crates/**/*.rs` below `root`.
fn scan_tree(root: &Path, allow: &[(&str, &str)]) -> Vec<Violation> {
    let mut paths = Vec::new();
    rust_files(&root.join("crates"), &mut paths);
    let files = paths
        .into_iter()
        .map(|file| {
            let relative = file
                .strip_prefix(root)
                .expect("file is under the root")
                .to_string_lossy()
                .replace('\\', "/");
            let text = fs::read_to_string(&file)
                .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
            (relative, text)
        })
        .collect();
    scan_files(&files, allow)
}

/// The manifest-dir lookup expression the fixtures embed.
fn lookup() -> &'static str {
    r#"env!("CARGO_MANIFEST_DIR")"#
}

/// Fixture source with `@` standing for the forbidden lookup.
fn source(template: &str) -> String {
    template.replace('@', lookup())
}

fn lines_of(path: &str, template: &str) -> Vec<usize> {
    analyze(path, &source(template), false)
        .violations
        .iter()
        .map(|violation| violation.line)
        .collect()
}

fn tree(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(path, template)| ((*path).to_owned(), source(template)))
        .collect()
}

#[test]
fn no_test_code_uses_a_compile_time_artifact_lookup() {
    let violations = scan_tree(&pohunek_test_support::workspace_root(), ALLOW_LIST);
    assert!(
        violations.is_empty(),
        "test code must resolve artifacts at run time through pohunek_test_support \
         (manifest_dir, workspace_root, bin_exe, worker_binary): {violations:#?}"
    );
}

#[test]
fn allow_list_entries_name_existing_files_and_state_a_reason() {
    let root = pohunek_test_support::workspace_root();
    for (path, reason) in ALLOW_LIST {
        assert!(root.join(path).is_file(), "{path} does not exist");
        assert!(!reason.trim().is_empty(), "{path} has no reason");
    }
}

#[test]
fn flags_a_bin_exe_lookup_in_an_integration_test() {
    let text = "let c = Command::new(env!(\"CARGO_BIN_EXE_tool\"));\n";
    let found = analyze("crates/demo/tests/run.rs", text, false).violations;
    assert_eq!(
        found,
        [Violation {
            path: "crates/demo/tests/run.rs".into(),
            line: 1,
            lookup: "env!(\"CARGO_BIN_EXE_tool\"..)".into(),
        }]
    );
}

#[test]
fn flags_a_lookup_in_test_named_source_files() {
    for path in [
        "crates/demo/src/tests.rs",
        "crates/demo/src/governance/owner_tests.rs",
        "crates/demo/src/session/tests/fixtures.rs",
    ] {
        assert_eq!(lines_of(path, "let p = @;\n"), [1], "{path}");
    }
}

#[test]
fn ignores_non_test_source_and_build_scripts() {
    for path in ["crates/demo/src/lib.rs", "crates/demo/build.rs"] {
        assert!(lines_of(path, "let p = @;\n").is_empty(), "{path}");
    }
}

#[test]
fn flags_only_the_body_of_an_inline_cfg_test_module() {
    let template = "fn tool() {\n    let p = @;\n}\n\n#[cfg(test)]\nmod tests {\n    fn helper() {\n        let p = @;\n    }\n}\n\nfn after() {\n    let p = @;\n}\n";
    assert_eq!(lines_of("crates/demo/src/lib.rs", template), [8]);
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
fn ignores_comments() {
    let pattern = lookup();
    let text = format!(
        "// {pattern}\n/* {pattern}\n   {pattern} */\nlet ok = 1; // {pattern}\nlet a = env /* c */ ! // c\n ( // c\n \"CARGO_PKG_NAME\");\n"
    );
    assert!(analyze("crates/demo/tests/run.rs", &text, false)
        .violations
        .is_empty());
}

#[test]
fn flags_every_macro_form_that_reads_a_forbidden_variable() {
    let forms = [
        r#"let p = env!("CARGO_MANIFEST_DIR", "must be set");"#,
        r#"let p = option_env!("CARGO_MANIFEST_DIR");"#,
        "let p = env\n    !\n    (\n        \"CARGO_MANIFEST_DIR\"\n    );",
        "let p = env /* split */ ! ( // note\n \"CARGO_MANIFEST_DIR\" );",
        r#"let p = std::env!("CARGO_MANIFEST_DIR");"#,
        r#"let p = ::core::env!("CARGO_MANIFEST_DIR");"#,
        r##"let p = env!(r#"CARGO_MANIFEST_DIR"#);"##,
        r#"let p = env!(r"CARGO_MANIFEST_DIR");"#,
        r#"let p = env!["CARGO_MANIFEST_DIR"];"#,
        r#"let p = env!("CARGO_BIN_EXE_pohunek");"#,
        r#"let p = option_env!("CARGO_BIN_EXE_pohunek-sessiond", "x");"#,
        r#"let p = std::option_env!("CARGO_BIN_EXE_");"#,
    ];
    for form in forms {
        let text = format!("fn f() {{\n{form}\n}}\n");
        let found = analyze("crates/demo/tests/run.rs", &text, false).violations;
        assert_eq!(found.len(), 1, "{form}: {found:?}");
        assert_eq!(found[0].line, 2, "{form}");
    }
}

#[test]
fn ignores_macros_and_text_that_are_not_forbidden_lookups() {
    let forms = [
        r#"let p = env!("CARGO_PKG_NAME");"#,
        r#"let p = option_env!("CARGO_PKG_VERSION", "x");"#,
        r#"let p = my_env!("CARGO_MANIFEST_DIR");"#,
        r#"let p = std::env::var("CARGO_MANIFEST_DIR");"#,
        r#"let p = env::var_os("CARGO_BIN_EXE_tool");"#,
        r#"let p = "env!(\"CARGO_MANIFEST_DIR\")";"#,
        r##"let p = r#"option_env!("CARGO_BIN_EXE_x")"#;"##,
        r#"let p = format!("{}", "CARGO_MANIFEST_DIR");"#,
        r#"let p = concat!("CARGO_MANIFEST_DIR");"#,
        r#"let p = env!(concat!("CARGO_MANIFEST", "_DIR"));"#,
        r#"let p = env!("NOT_CARGO_MANIFEST_DIR");"#,
        r#"let p = env!("CARGO_MANIFEST_DIR_EXTRA");"#,
    ];
    for form in forms {
        let text = format!("fn f() {{\n{form}\n}}\n");
        let found = analyze("crates/demo/tests/run.rs", &text, false).violations;
        assert!(found.is_empty(), "{form}: {found:?}");
    }
}

#[test]
fn a_double_slash_inside_a_string_does_not_hide_a_lookup() {
    let text = format!("let u = \"http://x\"; let p = {};\n", lookup());
    assert_eq!(
        analyze("crates/demo/tests/run.rs", &text, false)
            .violations
            .len(),
        1
    );
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
fn a_lookup_inside_a_raw_byte_or_c_string_is_not_a_lookup() {
    let literals = [
        r##"let s = br#"@"#;"##,
        r##"let s = br#"a" @"#;"##,
        r##"let s = cr#"a" @"#;"##,
        r###"let s = br##"a"# @"##;"###,
        r##"let s = cr#"@"#;"##,
        r###"let s = br##"@"##;"###,
        r#"let s = br"@";"#,
        r##"let s = r#"@"#;"##,
        r#"let s = b"@";"#,
        r#"let s = c"@";"#,
    ];
    for literal in literals {
        let template = literal.replace('@', "env!(\"CARGO_MANIFEST_DIR\")");
        assert!(
            lines_of("crates/demo/tests/run.rs", &format!("{template}\n")).is_empty(),
            "{literal}"
        );
    }
}

#[test]
fn identifiers_ending_in_literal_prefixes_do_not_open_raw_strings() {
    let template = "fn f() {\n    let bar = 1;\n    let c = bar;\n    let p = @;\n}\n#[cfg(test)]\nmod tests {\n    fn g() { let for_c = r#type; let p = @; }\n}\n";
    assert_eq!(lines_of("crates/demo/src/lib.rs", template), [8]);
    let template = "#[test]\nfn f() {\n    let aBr = 1;\n    let bbr = aBr;\n    let p = @;\n}\n";
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
fn an_external_cfg_test_module_file_is_scanned_even_with_a_plain_name() {
    let files = tree(&[
        (
            "crates/demo/src/lib.rs",
            "#[cfg(test)]\nmod helpers;\nfn tool() { let p = @; }\n",
        ),
        ("crates/demo/src/helpers.rs", "fn h() { let p = @; }\n"),
    ]);
    let found = scan_files(&files, &[]);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].path, "crates/demo/src/helpers.rs");
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
    let found: Vec<String> = scan_files(&files, &[])
        .into_iter()
        .map(|violation| violation.path)
        .collect();
    assert_eq!(
        found,
        [
            "crates/demo/src/a/deep.rs",
            "crates/demo/src/c/d.rs",
            "crates/demo/src/support/custom.rs",
        ]
    );
}

#[test]
fn an_external_module_in_a_non_test_module_file_stays_unscanned() {
    let files = tree(&[
        ("crates/demo/src/lib.rs", "mod helpers;\n"),
        ("crates/demo/src/helpers.rs", "fn h() { let p = @; }\n"),
    ]);
    assert!(scan_files(&files, &[]).is_empty());
}

#[test]
fn allow_list_exempts_only_the_named_file() {
    let files = tree(&[
        ("crates/demo/tests/kept.rs", "let p = @;\n"),
        ("crates/demo/tests/other.rs", "let p = @;\n"),
    ]);
    let allow = [("crates/demo/tests/kept.rs", "fixture reason")];
    let found = scan_files(&files, &allow);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].path, "crates/demo/tests/other.rs");
}
