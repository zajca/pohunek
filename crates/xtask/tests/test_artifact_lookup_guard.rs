//! Keeps compile-time artifact lookups out of test code.
//!
//! A test that bakes `CARGO_BIN_EXE_*` or `CARGO_MANIFEST_DIR` into the binary
//! at compile time breaks when a nextest archive is extracted at another
//! absolute path. Test code resolves binaries, source files and the session
//! worker at run time through `pohunek_test_support` instead.
//!
//! Scanned as test code: every `.rs` file under a `tests` directory, files
//! named `tests.rs` or `*_tests.rs`, and the body of a `#[cfg(test)] mod`
//! block (ended by the closing brace at the same indentation, which rustfmt
//! guarantees). Build scripts and non-test `src/` code are not scanned; the
//! tool's own workspace lookup in `crates/xtask/src/lib.rs` is such code.

// Rust guideline compliant 2026-09-30

use std::fs;
use std::path::{Path, PathBuf};

/// Forbidden compile-time lookups. Assembled with `concat!` so this file does
/// not contain the literals it searches for.
const FORBIDDEN_PATTERNS: [&str; 2] = [
    concat!("env!(", "\"CARGO_BIN_EXE_"),
    concat!("env!(", "\"CARGO_MANIFEST_DIR\")"),
];

/// Files allowed to contain a forbidden pattern, as `(workspace-relative path,
/// reason)`. No file needs an exemption today; an entry must state why the
/// lookup cannot happen at run time.
const ALLOW_LIST: &[(&str, &str)] = &[];

/// One forbidden lookup found in test code.
#[derive(Debug, PartialEq, Eq)]
struct Violation {
    path: String,
    line: usize,
    pattern: &'static str,
}

/// Whether the whole file is test code judged by its path alone.
fn is_test_file(path: &str) -> bool {
    let mut parts: Vec<&str> = path.split('/').collect();
    let Some(file) = parts.pop() else {
        return false;
    };
    parts.contains(&"tests") || file == "tests.rs" || file.ends_with("_tests.rs")
}

/// Returns each line of `text` with comments removed. String literals are
/// tracked per line so a `//` inside one does not start a comment; block
/// comments carry across lines.
fn strip_comments(text: &str) -> Vec<String> {
    let mut in_block = false;
    text.lines()
        .map(|line| {
            let mut code = String::new();
            let mut in_string = false;
            let mut chars = line.chars().peekable();
            while let Some(c) = chars.next() {
                if in_block {
                    if c == '*' && chars.peek() == Some(&'/') {
                        chars.next();
                        in_block = false;
                    }
                    continue;
                }
                if in_string {
                    code.push(c);
                    if c == '\\' {
                        if let Some(escaped) = chars.next() {
                            code.push(escaped);
                        }
                    } else if c == '"' {
                        in_string = false;
                    }
                    continue;
                }
                match (c, chars.peek()) {
                    ('/', Some('/')) => break,
                    ('/', Some('*')) => {
                        chars.next();
                        in_block = true;
                    }
                    ('"', _) => {
                        in_string = true;
                        code.push(c);
                    }
                    _ => code.push(c),
                }
            }
            code
        })
        .collect()
}

/// Marks the lines inside `#[cfg(test)] mod name { ... }` blocks.
fn cfg_test_module_lines(lines: &[String]) -> Vec<bool> {
    let mut inside = vec![false; lines.len()];
    let mut index = 0;
    while index < lines.len() {
        if lines[index].trim() != "#[cfg(test)]" {
            index += 1;
            continue;
        }
        let declaration = index + 1;
        let Some(line) = lines.get(declaration) else {
            break;
        };
        let trimmed = line.trim_start();
        let is_block_module = (trimmed.starts_with("mod ") || trimmed.starts_with("pub mod "))
            && trimmed.trim_end().ends_with('{');
        if !is_block_module {
            index += 1;
            continue;
        }
        let indent = line.len() - trimmed.len();
        let closing = format!("{}}}", " ".repeat(indent));
        let end = (declaration + 1..lines.len())
            .find(|&i| lines[i].trim_end() == closing)
            .unwrap_or(lines.len() - 1);
        for flag in &mut inside[declaration..=end] {
            *flag = true;
        }
        index = end + 1;
    }
    inside
}

/// Finds forbidden lookups in the test-side code of one file.
fn scan_file(path: &str, text: &str) -> Vec<Violation> {
    let lines = strip_comments(text);
    let whole_file = is_test_file(path);
    let test_module = if whole_file {
        Vec::new()
    } else {
        cfg_test_module_lines(&lines)
    };
    let mut found = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if !whole_file && !test_module[index] {
            continue;
        }
        for pattern in FORBIDDEN_PATTERNS {
            if line.contains(pattern) {
                found.push(Violation {
                    path: path.to_owned(),
                    line: index + 1,
                    pattern,
                });
            }
        }
    }
    found
}

/// Whether `path` is exempt according to `allow`.
fn is_allowed(path: &str, allow: &[(&str, &str)]) -> bool {
    allow.iter().any(|(allowed, _)| *allowed == path)
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
    let mut files = Vec::new();
    rust_files(&root.join("crates"), &mut files);
    files.sort();
    let mut found = Vec::new();
    for file in files {
        let relative = file
            .strip_prefix(root)
            .expect("file is under the root")
            .to_string_lossy()
            .replace('\\', "/");
        if is_allowed(&relative, allow) {
            continue;
        }
        let text =
            fs::read_to_string(&file).unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        found.extend(scan_file(&relative, &text));
    }
    found
}

fn manifest_lookup() -> String {
    format!("let p = {}).join(\"x\");\n", FORBIDDEN_PATTERNS[1])
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
    let text = format!("let c = Command::new({}x\"));\n", FORBIDDEN_PATTERNS[0]);
    let found = scan_file("crates/demo/tests/run.rs", &text);
    assert_eq!(
        found,
        [Violation {
            path: "crates/demo/tests/run.rs".into(),
            line: 1,
            pattern: FORBIDDEN_PATTERNS[0],
        }]
    );
}

#[test]
fn flags_a_manifest_dir_lookup_in_test_named_source_files() {
    for path in [
        "crates/demo/src/tests.rs",
        "crates/demo/src/governance/owner_tests.rs",
        "crates/demo/src/session/tests/fixtures.rs",
    ] {
        assert_eq!(scan_file(path, &manifest_lookup()).len(), 1, "{path}");
    }
}

#[test]
fn flags_a_lookup_inside_a_cfg_test_module_only() {
    let lookup = manifest_lookup();
    let text = format!(
        "fn tool() {{\n    {lookup}}}\n\n#[cfg(test)]\nmod tests {{\n    fn helper() {{\n        {lookup}    }}\n}}\n\nfn after() {{\n    {lookup}}}\n"
    );
    let found = scan_file("crates/demo/src/lib.rs", &text);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].line, 8);
}

#[test]
fn ignores_non_test_source_and_build_scripts() {
    for path in ["crates/demo/src/lib.rs", "crates/demo/build.rs"] {
        assert!(scan_file(path, &manifest_lookup()).is_empty(), "{path}");
    }
}

#[test]
fn ignores_comments() {
    let pattern = FORBIDDEN_PATTERNS[1];
    let text = format!("// {pattern}\n/* {pattern}\n   {pattern} */\nlet ok = 1; // {pattern}\n");
    assert!(scan_file("crates/demo/tests/run.rs", &text).is_empty());
}

#[test]
fn a_double_slash_inside_a_string_does_not_hide_a_lookup() {
    let text = format!("let u = \"http://x\"; let p = {};\n", FORBIDDEN_PATTERNS[1]);
    assert_eq!(scan_file("crates/demo/tests/run.rs", &text).len(), 1);
}

#[test]
fn allow_list_exempts_only_the_named_file() {
    let dir = pohunek_test_support::tempdir().expect("fixture root");
    let tests = dir.path().join("crates/demo/tests");
    fs::create_dir_all(&tests).expect("create tests dir");
    fs::write(tests.join("kept.rs"), manifest_lookup()).expect("write kept");
    fs::write(tests.join("other.rs"), manifest_lookup()).expect("write other");

    let allow = [("crates/demo/tests/kept.rs", "fixture reason")];
    let found = scan_tree(dir.path(), &allow);

    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].path, "crates/demo/tests/other.rs");
}
