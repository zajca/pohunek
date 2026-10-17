//! Keeps literal `--exact` test filters out of integration test code.
//!
//! An integration test that re-runs its own test binary selects one test with
//! `--exact <name>`. libtest names a test by its path below the crate root, so
//! a bare name is right only while the test file is its own test binary; once
//! the file is a module of a larger binary, the filter matches nothing and the
//! parent test waits for a child that never ran. Integration test code builds
//! the filter with `pohunek_test_support::exact_test_filter(module_path!(), ..)`
//! instead, which is right in both layouts.
//!
//! Only files below `crates/<package>/tests/` are scanned: a unit test in
//! `src/` is named by its full module path, which the integration test layout
//! never changes. What counts as test code is decided by `xtask::test_code`.

// Rust guideline compliant 2026-10-17

use std::collections::BTreeMap;

use xtask::test_code::{classify, read_sources, SourceFile};

/// The libtest flag whose literal spelling is refused.
const EXACT_FLAG: &str = "--exact";

/// Files allowed to spell the flag, as `(workspace-relative path, reason)`.
const ALLOW_LIST: &[(&str, &str)] = &[(
    "crates/xtask/tests/exact_filter_guard.rs",
    "the guard's own pattern and fixtures",
)];

/// One literal `--exact` found in integration test code.
#[derive(Debug, PartialEq, Eq)]
struct Violation {
    path: String,
    line: usize,
}

/// Whether `path` is a file below an integration test directory,
/// `crates/<package>/tests/`.
fn is_integration_test_path(path: &str) -> bool {
    let mut parts = path.split('/');
    parts.next() == Some("crates") && parts.next().is_some() && parts.next() == Some("tests")
}

/// Literal `--exact` occurrences in the test code of `file`, in line order.
/// The skeleton blanks string literals, so an occurrence the comment-free view
/// has and the skeleton lacks sits inside a string literal; comments are in
/// neither view.
fn violations_in(file: &SourceFile) -> Vec<Violation> {
    if !is_integration_test_path(file.path()) {
        return Vec::new();
    }
    let views = file.views();
    views
        .code
        .match_indices(EXACT_FLAG)
        .map(|(offset, _)| offset)
        .filter(|&offset| views.skeleton.get(offset..offset + EXACT_FLAG.len()) != Some(EXACT_FLAG))
        .filter(|&offset| file.in_test_code(offset))
        .map(|offset| Violation {
            path: file.path().to_owned(),
            line: file.line_of(offset),
        })
        .collect()
}

fn scan_files(files: &BTreeMap<String, String>) -> Vec<Violation> {
    classify(files)
        .values()
        .filter(|file| {
            !ALLOW_LIST
                .iter()
                .any(|(allowed, _)| *allowed == file.path())
        })
        .flat_map(violations_in)
        .collect()
}

fn analyze(path: &str, text: &str) -> Vec<Violation> {
    scan_files(&BTreeMap::from([(path.to_owned(), text.to_owned())]))
}

fn lines(path: &str, text: &str) -> Vec<usize> {
    analyze(path, text)
        .iter()
        .map(|violation| violation.line)
        .collect()
}

#[test]
fn the_workspace_has_no_literal_exact_filter_in_integration_tests() {
    let files = read_sources(&pohunek_test_support::workspace_root())
        .unwrap_or_else(|e| panic!("read workspace sources: {e}"));
    let violations = scan_files(&files);
    assert!(
        violations.is_empty(),
        "literal {EXACT_FLAG} in integration test code; build the filter with \
         pohunek_test_support::exact_test_filter(module_path!(), \"<test>\"): {violations:#?}"
    );
}

#[test]
fn a_literal_exact_filter_in_an_integration_test_is_refused() {
    let text = "#[test]\nfn parent() {\n    let _ = [\"--exact\", \"child\"];\n}\n";
    assert_eq!(lines("crates/demo/tests/reexec.rs", text), [3]);
    // Inside a format string, as a shell command line builds it.
    let text = "#[test]\nfn parent() {\n    let _ = format!(\"'{}' --exact child\", 1);\n}\n";
    assert_eq!(lines("crates/demo/tests/reexec.rs", text), [3]);
    // In a support module of the test directory.
    let text = "pub fn run() -> [&'static str; 2] {\n    [\"--exact\", \"child\"]\n}\n";
    assert_eq!(lines("crates/demo/tests/support/reexec.rs", text), [2]);
}

#[test]
fn the_helper_comments_and_unit_tests_are_not_refused() {
    let helper = "#[test]\nfn parent() {\n    let _ = pohunek_test_support::exact_test_filter(module_path!(), \"child\");\n}\n";
    assert_eq!(
        lines("crates/demo/tests/reexec.rs", helper),
        Vec::<usize>::new()
    );
    let comment = "// Under `--exact` the marker matches no test.\n#[test]\nfn parent() {}\n";
    assert_eq!(
        lines("crates/demo/tests/reexec.rs", comment),
        Vec::<usize>::new()
    );
    let unit = "#[cfg(test)]\nmod tests {\n    #[test]\n    fn parent() {\n        let _ = [\"--exact\", \"tests::child\"];\n    }\n}\n";
    assert_eq!(lines("crates/demo/src/lib.rs", unit), Vec::<usize>::new());
}
