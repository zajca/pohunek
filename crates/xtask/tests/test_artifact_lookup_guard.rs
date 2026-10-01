//! Keeps compile-time artifact lookups out of test code.
//!
//! A test that bakes `CARGO_BIN_EXE_*` or `CARGO_MANIFEST_DIR` into the binary
//! at compile time breaks when a nextest archive is extracted at another
//! absolute path. Test code resolves binaries, source files and the session
//! worker at run time through `pohunek_test_support` instead.
//!
//! What counts as test code is decided by `xtask::test_code`. Build scripts and
//! non-test `src/` code are not scanned; the tool's own workspace lookup in
//! `crates/xtask/src/lib.rs` is such code.

// Rust guideline compliant 2026-10-01

use std::collections::BTreeMap;
use std::path::Path;

use xtask::test_code::{
    classify, is_ident, read_sources, skip_whitespace, string_literal_at, SourceFile,
};

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

/// Finds `env!` and `option_env!` invocations, with any path prefix and any
/// whitespace or comments between tokens, whose first argument is a string
/// literal naming `CARGO_MANIFEST_DIR` or a `CARGO_BIN_EXE_*` variable. Macro
/// names are located in the skeleton so text inside string literals is never
/// taken for an invocation. Returns the offset of the macro name and a
/// description of the lookup.
fn forbidden_lookups(file: &SourceFile) -> Vec<(usize, String)> {
    let views = file.views();
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

/// Forbidden lookups that sit in test code of `file`, in line order.
fn violations_in(file: &SourceFile) -> Vec<Violation> {
    let mut violations: Vec<Violation> = forbidden_lookups(file)
        .into_iter()
        .filter(|&(offset, _)| file.in_test_code(offset))
        .map(|(offset, lookup)| Violation {
            path: file.path().to_owned(),
            line: file.line_of(offset),
            lookup,
        })
        .collect();
    violations.sort_by_key(|violation| violation.line);
    violations
}

/// Scans `files` (workspace-relative path to text): a file is test code when
/// its path says so or an external `mod` in test code declares it.
fn scan_files(files: &BTreeMap<String, String>, allow: &[(&str, &str)]) -> Vec<Violation> {
    classify(files)
        .values()
        .filter(|file| !allow.iter().any(|(allowed, _)| *allowed == file.path()))
        .flat_map(violations_in)
        .collect()
}

/// Scans `crates/**/*.rs` below `root`.
fn scan_tree(root: &Path, allow: &[(&str, &str)]) -> Vec<Violation> {
    let files = read_sources(root).unwrap_or_else(|e| panic!("read workspace sources: {e}"));
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

/// Violations found in the single file `path` holding `text`.
fn analyze(path: &str, text: &str) -> Vec<Violation> {
    let files = BTreeMap::from([(path.to_owned(), text.to_owned())]);
    scan_files(&files, &[])
}

fn lines_of(path: &str, template: &str) -> Vec<usize> {
    analyze(path, &source(template))
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
    let found = analyze("crates/demo/tests/run.rs", text);
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
fn ignores_comments() {
    let pattern = lookup();
    let text = format!(
        "// {pattern}\n/* {pattern}\n   {pattern} */\nlet ok = 1; // {pattern}\nlet a = env /* c */ ! // c\n ( // c\n \"CARGO_PKG_NAME\");\n"
    );
    assert!(analyze("crates/demo/tests/run.rs", &text).is_empty());
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
        let found = analyze("crates/demo/tests/run.rs", &text);
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
        let found = analyze("crates/demo/tests/run.rs", &text);
        assert!(found.is_empty(), "{form}: {found:?}");
    }
}

#[test]
fn a_double_slash_inside_a_string_does_not_hide_a_lookup() {
    let text = format!("let u = \"http://x\"; let p = {};\n", lookup());
    assert_eq!(analyze("crates/demo/tests/run.rs", &text).len(), 1);
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
