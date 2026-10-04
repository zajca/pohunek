//! Keeps per-agent special cases out of the daemon's production code.
//!
//! Launch, resume, fork, input framing, detection, probes, profile bases and
//! inventory resolve an agent through the runtime registry, so production code
//! under `crates/daemon/src` must not name the built-in agents `codex`,
//! `claude` or `hermes`. A name in a string literal or an identifier (for
//! example `AgentKind::Codex` or `run_hermes_version_probe`) is a finding, in
//! any letter case; comments and test code (as classified by
//! `xtask::test_code`) are not scanned.
//!
//! The names that remain are data or work owned by another issue. Each
//! allow-list entry pins the number of source lines that still name an agent
//! in one file and states which issue removes them (or why they are data), so
//! a new occurrence fails the scan and a removed one fails it until the entry
//! shrinks or goes away.

// Rust guideline compliant 2026-10-04

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use xtask::test_code::{classify, read_sources, SourceFile};

/// Production sources the scan covers, as a workspace-relative path prefix.
const SCANNED_PREFIX: &str = "crates/daemon/src/";

/// Agent names that must not appear in production code (lowercase).
const AGENT_NAMES: [&str; 3] = ["codex", "claude", "hermes"];

/// Who removes an allow-listed occurrence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    /// Compiled descriptor data that becomes package data when the built-in
    /// Codex, Claude and Hermes packages exist.
    Packages,
    /// Integration handlers (#144).
    Integration,
    /// Transcript parsing of external sessions (#487).
    Transcripts,
    /// Data tables that stay data: reserved ids and wire spellings.
    Data,
}

impl Owner {
    fn issue(self) -> &'static str {
        match self {
            Self::Packages => "#145 #146 #147",
            Self::Integration => "#144",
            Self::Transcripts => "#487",
            Self::Data => "data",
        }
    }
}

/// One file that may still name an agent: `(workspace-relative path, number of
/// source lines naming one, owner, reason)`.
type Entry = (&'static str, usize, Owner, &'static str);

const ALLOW_LIST: &[Entry] = &[
    (
        "crates/daemon/src/agent/host/builtin.rs",
        6,
        Owner::Packages,
        "embedded built-in descriptors and their manifest name table",
    ),
    (
        "crates/daemon/src/agent/host/definition.rs",
        4,
        Owner::Data,
        "wire spelling of the `hermes_safe_text` input text policy",
    ),
    (
        "crates/daemon/src/agent/host/registry.rs",
        1,
        Owner::Data,
        "RESERVED_RUNTIME_IDS: ids no other source may claim",
    ),
    (
        "crates/daemon/src/agent/mod.rs",
        4,
        Owner::Data,
        "the Hermes safe-text input policy a descriptor selects by name",
    ),
    (
        "crates/daemon/src/agent/profile.rs",
        1,
        Owner::Data,
        "recovery hint listing the built-in runtime ids",
    ),
    (
        "crates/daemon/src/capabilities.rs",
        40,
        Owner::Packages,
        "compiled Hermes version probe selected by a descriptor's probe name",
    ),
    (
        "crates/daemon/src/detect/mod.rs",
        9,
        Owner::Packages,
        "compiled detection manifests of the built-in descriptors",
    ),
    (
        "crates/daemon/src/external/mod.rs",
        12,
        Owner::Transcripts,
        "external transcript roots and parsing per provider",
    ),
    (
        "crates/daemon/src/integration/doctor.rs",
        13,
        Owner::Integration,
        "integration handlers",
    ),
    (
        "crates/daemon/src/integration/mod.rs",
        225,
        Owner::Integration,
        "integration handlers",
    ),
    (
        "crates/daemon/src/integration/uninstall.rs",
        58,
        Owner::Integration,
        "integration handlers",
    ),
    (
        "crates/daemon/src/notifications/mod.rs",
        2,
        Owner::Packages,
        "provider names that select notification sources",
    ),
    (
        "crates/daemon/src/notifications/policy.rs",
        3,
        Owner::Packages,
        "default per-provider notification policy",
    ),
    (
        "crates/daemon/src/session/reconcile.rs",
        5,
        Owner::Packages,
        "worker provider strings mapped to agent kinds until the AgentKind to RuntimeRef swap and the built-in packages land",
    ),
];

/// Lines of `file` that name a built-in agent outside comments and test code.
fn finding_lines(file: &SourceFile) -> BTreeSet<usize> {
    let code = &file.views().code;
    let lowered = code.to_ascii_lowercase();
    AGENT_NAMES
        .iter()
        .flat_map(|name| lowered.match_indices(name).map(|(offset, _)| offset))
        .filter(|&offset| !file.in_test_code(offset))
        .map(|offset| file.line_of(offset))
        .collect()
}

/// Line counts per scanned file for `files` (workspace-relative path to text).
fn scan_files(files: &BTreeMap<String, String>) -> BTreeMap<String, BTreeSet<usize>> {
    classify(files)
        .values()
        .filter(|file| file.path().starts_with(SCANNED_PREFIX))
        .map(|file| (file.path().to_owned(), finding_lines(file)))
        .filter(|(_, lines)| !lines.is_empty())
        .collect()
}

fn scan_tree(root: &Path) -> BTreeMap<String, BTreeSet<usize>> {
    let files = read_sources(root).unwrap_or_else(|e| panic!("read workspace sources: {e}"));
    scan_files(&files)
}

/// Compares `found` with `allow`: unlisted files, files above their pinned
/// count (new occurrences) and files below it (stale entries).
fn mismatches(found: &BTreeMap<String, BTreeSet<usize>>, allow: &[Entry]) -> Vec<String> {
    let mut problems = Vec::new();
    for (path, lines) in found {
        match allow.iter().find(|entry| entry.0 == path) {
            None => problems.push(format!(
                "{path}: names a built-in agent on lines {lines:?}; resolve agents through the \
                 runtime registry"
            )),
            Some(&(_, pinned, owner, _)) if lines.len() > pinned => problems.push(format!(
                "{path}: {} lines name a built-in agent, {pinned} are allowed for {}: {lines:?}",
                lines.len(),
                owner.issue()
            )),
            Some(&(_, pinned, _, _)) if lines.len() < pinned => problems.push(format!(
                "{path}: only {} lines name a built-in agent; lower the allow-list entry from \
                 {pinned}",
                lines.len()
            )),
            Some(_) => {}
        }
    }
    for (path, ..) in allow {
        if !found.contains_key(*path) {
            problems.push(format!(
                "{path}: no longer names a built-in agent; remove its entry"
            ));
        }
    }
    problems
}

#[test]
fn production_code_names_no_builtin_agent_outside_the_allow_list() {
    let found = scan_tree(&pohunek_test_support::workspace_root());
    let problems = mismatches(&found, ALLOW_LIST);
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn allow_list_entries_are_unique_existing_files_with_a_reason() {
    let root = pohunek_test_support::workspace_root();
    let mut seen = BTreeSet::new();
    for (path, pinned, _, reason) in ALLOW_LIST {
        assert!(path.starts_with(SCANNED_PREFIX), "{path} is not scanned");
        assert!(root.join(path).is_file(), "{path} does not exist");
        assert!(seen.insert(*path), "{path} is listed twice");
        assert!(*pinned > 0, "{path} pins no lines");
        assert!(!reason.trim().is_empty(), "{path} has no reason");
    }
}

fn scan_one(path: &str, text: &str) -> BTreeSet<usize> {
    let files = BTreeMap::from([(path.to_owned(), text.to_owned())]);
    scan_files(&files).remove(path).unwrap_or_default()
}

#[test]
fn flags_literals_and_identifiers_in_any_letter_case() {
    let text = "fn f() {\n    let a = \"codex\";\n    let b = AgentKind::Claude;\n    let c = run_hermes_probe();\n    let d = \"HERMES\";\n}\n";
    assert_eq!(
        scan_one("crates/daemon/src/x.rs", text),
        BTreeSet::from([2, 3, 4, 5])
    );
}

#[test]
fn counts_a_line_once_however_often_it_names_an_agent() {
    let text = "const T: [&str; 3] = [\"codex\", \"claude\", \"hermes\"];\n";
    assert_eq!(
        scan_one("crates/daemon/src/x.rs", text),
        BTreeSet::from([1])
    );
}

#[test]
fn ignores_comments_and_test_code() {
    let text = "// codex\n/// claude\n/* hermes */\nfn f() {}\n\n#[cfg(test)]\nmod tests {\n    fn t() {\n        let a = \"codex\";\n    }\n}\n";
    assert!(scan_one("crates/daemon/src/x.rs", text).is_empty());
    assert!(scan_one("crates/daemon/src/session/tests.rs", "let a = \"codex\";\n").is_empty());
}

#[test]
fn ignores_files_outside_the_daemon_sources() {
    assert!(scan_one("crates/cli/src/x.rs", "let a = \"codex\";\n").is_empty());
    assert!(scan_one("crates/daemon/tests/x.rs", "let a = \"codex\";\n").is_empty());
}

#[test]
fn reports_new_and_stale_entries() {
    let found = BTreeMap::from([
        ("crates/daemon/src/a.rs".to_owned(), BTreeSet::from([1, 2])),
        ("crates/daemon/src/b.rs".to_owned(), BTreeSet::from([1])),
        ("crates/daemon/src/c.rs".to_owned(), BTreeSet::from([7])),
    ]);
    let allow: &[Entry] = &[
        ("crates/daemon/src/a.rs", 1, Owner::Data, "grew"),
        ("crates/daemon/src/b.rs", 3, Owner::Data, "shrank"),
        ("crates/daemon/src/gone.rs", 1, Owner::Data, "removed"),
    ];
    let problems = mismatches(&found, allow);
    assert_eq!(problems.len(), 4, "{problems:#?}");
    assert!(problems
        .iter()
        .any(|p| p.contains("a.rs") && p.contains("1 are allowed")));
    assert!(problems
        .iter()
        .any(|p| p.contains("b.rs") && p.contains("lower the allow-list")));
    assert!(problems
        .iter()
        .any(|p| p.contains("c.rs") && p.contains("runtime registry")));
    assert!(problems
        .iter()
        .any(|p| p.contains("gone.rs") && p.contains("remove its entry")));
}

#[test]
fn an_exact_allow_list_has_no_mismatch() {
    let found = BTreeMap::from([("crates/daemon/src/a.rs".to_owned(), BTreeSet::from([1, 2]))]);
    let allow: &[Entry] = &[("crates/daemon/src/a.rs", 2, Owner::Data, "data")];
    assert!(mismatches(&found, allow).is_empty());
}
