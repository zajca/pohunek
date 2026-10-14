//! Knowledge-log fragments through `cargo xtask docs`: the release fold, the
//! migration of a shared-log branch, the assembled log of `docs build`, and
//! the validation of a fragment.

// Rust guideline compliant 2026-10-14

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use knowledge::BundleValidationIssue;
use xtask::{build_docs, fold_log, migrate_log, validate_docs, BuildOptions, XtaskError};

/// Mode of a directory its owner can list but not write to.
const READ_ONLY_DIR_MODE: u32 = 0o500;

/// Release section the scenarios fold into.
const RELEASE: &str = "0.35.0";

/// A log as committed once fragments exist: a title, an introduction and the
/// sections of earlier releases.
const COMMITTED_LOG: &str =
    "# Knowledge Bundle Log\n\nIntroduction.\n\n## 0.34.2\n\n- Earlier release.\n";

/// The oldest fragment, written first so the directory order differs from the
/// log order.
const OLDER: (&str, &str) = (
    "100-older-change.md",
    "## Unreleased (2026-10-13, older change, #100)\n\n- Older body.\n",
);

/// A newer fragment whose issue sorts after [`SAME_DAY`]'s.
const NEWER: (&str, &str) = (
    "200-newer-change.md",
    "## Unreleased (2026-10-14, newer change, #200)\n\n- Newer body.\n",
);

/// A fragment of the same date as [`NEWER`] with a lower issue number.
const SAME_DAY: (&str, &str) = (
    "50-same-day-change.md",
    "## Unreleased (2026-10-14, same-day change, #50 #60)\n\n- Same-day body.\n",
);

/// A private bundle directory that is removed when the guard drops.
fn temp_dir(name: &str) -> tempfile::TempDir {
    pohunek_test_support::tempdir_with_prefix(&format!("pxl-{name}-")).expect("private fixture dir")
}

/// Writes a minimal valid source bundle with `log` and the `fragments`.
fn write_bundle(root: &Path, log: &str, fragments: &[(&str, &str)]) {
    fs::write(root.join("index.md"), "# Index\n").expect("write index");
    fs::write(root.join("log.md"), log).expect("write log");
    if fragments.is_empty() {
        return;
    }
    fs::create_dir_all(root.join("log.d")).expect("create log.d");
    for (name, content) in fragments {
        fs::write(root.join("log.d").join(name), content).expect("write fragment");
    }
}

fn read(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path).expect("read file")
}

/// Every file under `root`, with its bytes, in path order.
fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).expect("read dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                pending.push(path);
            } else {
                let bytes = fs::read(&path).expect("read file");
                files.push((path, bytes));
            }
        }
    }
    files.sort();
    files
}

#[test]
fn release_fold_files_fragments_under_one_section_in_log_order_and_removes_them() {
    let dir = temp_dir("fold");
    let root = dir.path();
    write_bundle(root, COMMITTED_LOG, &[OLDER, NEWER, SAME_DAY]);

    let summary = fold_log(root, RELEASE, false).expect("fold succeeds");

    assert_eq!(
        summary.folded,
        [SAME_DAY.0, NEWER.0, OLDER.0],
        "newest date first, then the file's issue"
    );
    assert_eq!(
        read(root.join("log.md")),
        concat!(
            "# Knowledge Bundle Log\n\nIntroduction.\n\n",
            "## 0.35.0\n\n",
            "### 2026-10-14, same-day change, #50 #60\n\n- Same-day body.\n\n",
            "### 2026-10-14, newer change, #200\n\n- Newer body.\n\n",
            "### 2026-10-13, older change, #100\n\n- Older body.\n\n",
            "## 0.34.2\n\n- Earlier release.\n",
        )
    );
    assert!(
        !root.join("log.d").exists(),
        "the fold leaves no fragment or empty fragment directory behind"
    );
    validate_docs(root).expect("the folded bundle validates");
}

#[test]
fn release_fold_dry_run_changes_nothing() {
    let dir = temp_dir("fold-dry");
    let root = dir.path();
    write_bundle(root, COMMITTED_LOG, &[OLDER, NEWER]);
    let before = snapshot(root);

    let summary = fold_log(root, RELEASE, true).expect("dry run succeeds");

    assert_eq!(summary.folded, [NEWER.0, OLDER.0]);
    assert_eq!(
        snapshot(root),
        before,
        "a dry run writes and removes nothing"
    );
}

#[test]
fn release_fold_refuses_a_log_that_still_carries_an_unreleased_section() {
    let dir = temp_dir("fold-unreleased");
    let root = dir.path();
    let log = "# Knowledge Bundle Log\n\n## Unreleased (2026-10-14, stray, #7)\n\n- Stray.\n";
    write_bundle(root, log, &[NEWER]);
    let before = snapshot(root);

    let error = fold_log(root, RELEASE, false).expect_err("a stray unreleased section is refused");

    assert!(matches!(error, XtaskError::KnowledgeLog(_)), "{error}");
    assert!(
        error.to_string().contains("belongs in a fragment"),
        "{error}"
    );
    assert_eq!(snapshot(root), before, "a refused fold changes nothing");
}

#[test]
fn migrate_moves_every_unreleased_section_into_a_fragment_the_fold_reads_back() {
    let dir = temp_dir("migrate");
    let root = dir.path();
    let log = concat!(
        "# Knowledge Bundle Log\n\n",
        "## Unreleased (2026-10-14, newer change, #200)\n\n- Newer body.\n\n",
        "## Unreleased (2026-10-14, newer change, #200)\n\n- Second entry of the issue.\n\n",
        "## Unreleased (2026-10-13, older change: with punctuation, #100)\n\n- Older body.\n",
        "  continued line.\n\n",
        "## 0.34.2\n\n- Earlier release.\n",
    );
    write_bundle(root, log, &[]);

    let summary = migrate_log(root, false).expect("migrate succeeds");

    assert_eq!(
        summary.written,
        [
            "200-newer-change.md",
            "200-newer-change-2.md",
            "100-older-change-with-punctuation.md",
        ]
    );
    assert_eq!(
        read(root.join("log.md")),
        "# Knowledge Bundle Log\n\n## 0.34.2\n\n- Earlier release.\n"
    );
    assert_eq!(
        read(root.join("log.d/100-older-change-with-punctuation.md")),
        "## Unreleased (2026-10-13, older change: with punctuation, #100)\n\n- Older body.\n  continued line.\n"
    );
    validate_docs(root).expect("the migrated bundle validates");

    fold_log(root, RELEASE, false).expect("the fold reads the migrated fragments");
    let folded = read(root.join("log.md"));
    let newer = folded
        .find("- Newer body.")
        .expect("first #200 entry folded");
    let second = folded
        .find("- Second entry of the issue.")
        .expect("second #200 entry folded");
    let older = folded.find("- Older body.").expect("#100 entry folded");
    assert!(
        newer < older && second < older,
        "newest date first: {folded}"
    );
}

#[test]
fn migrate_refuses_a_section_without_an_issue_and_writes_nothing() {
    let dir = temp_dir("migrate-no-issue");
    let root = dir.path();
    let log = "# Knowledge Bundle Log\n\n## Unreleased (2026-10-14, nameless)\n\n- Body.\n";
    write_bundle(root, log, &[]);
    let before = snapshot(root);

    let error = migrate_log(root, false).expect_err("an unnamed section is refused");

    assert!(error.to_string().contains("cites no issue"), "{error}");
    assert_eq!(snapshot(root), before);
}

#[test]
fn docs_build_ships_the_fragments_inside_the_assembled_log_only() {
    let dir = temp_dir("build");
    let source = dir.path().join("source");
    fs::create_dir_all(&source).expect("create source");
    write_bundle(&source, COMMITTED_LOG, &[OLDER, NEWER]);

    let summary = build_docs(BuildOptions {
        source_dir: source.clone(),
        output_root: dir.path().join("out"),
        pohunek_version: "0.0.0-test".to_owned(),
    })
    .expect("build succeeds");

    assert_eq!(
        read(summary.bundle_dir.join("log.md")),
        concat!(
            "# Knowledge Bundle Log\n\nIntroduction.\n\n",
            "## Unreleased (2026-10-14, newer change, #200)\n\n- Newer body.\n\n",
            "## Unreleased (2026-10-13, older change, #100)\n\n- Older body.\n\n",
            "## 0.34.2\n\n- Earlier release.\n",
        )
    );
    assert!(!summary.bundle_dir.join("log.d").exists());
    assert_eq!(
        read(source.join("log.md")),
        COMMITTED_LOG,
        "the source log is untouched"
    );
}

#[test]
fn a_fragment_whose_heading_does_not_cite_its_file_issue_is_refused() {
    let dir = temp_dir("bad-fragment");
    let root = dir.path();
    let wrong = (
        "300-wrong-issue.md",
        "## Unreleased (2026-10-14, wrong issue, #301)\n\n- Body.\n",
    );
    write_bundle(root, COMMITTED_LOG, &[wrong]);

    let error = validate_docs(root).expect_err("the fragment is refused");

    let XtaskError::BundleValidation(validation) = &error else {
        panic!("unexpected error: {error}");
    };
    assert!(
        validation.issues().iter().any(|issue| matches!(
            issue,
            BundleValidationIssue::LogFragment { reason, .. } if reason.contains("#300")
        )),
        "{error}"
    );
}

#[test]
fn adding_a_fragment_to_the_committed_bundle_touches_no_other_file() {
    let dir = temp_dir("committed-plus-one");
    let source = pohunek_test_support::workspace_root().join("docs/knowledge");
    let copy = dir.path().join("knowledge");
    copy_tree(&source, &copy);
    let before = validate_docs(&copy).expect("the committed bundle validates");

    let added = (
        "9999-added-change.md",
        "## Unreleased (2026-12-31, added change, #9999)\n\n- Added.\n",
    );
    fs::create_dir_all(copy.join("log.d")).expect("create log.d");
    fs::write(copy.join("log.d").join(added.0), added.1).expect("write fragment");
    let after = validate_docs(&copy).expect("the bundle with one more fragment validates");

    assert_eq!(
        after.files_checked, before.files_checked,
        "a fragment does not change the exact bundle file count"
    );
    assert_eq!(after.concept_count, before.concept_count);
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).expect("create copy dir");
    for entry in fs::read_dir(source).expect("read source dir") {
        let path = entry.expect("source entry").path();
        let target = destination.join(path.file_name().expect("entry name"));
        if path.is_dir() {
            copy_tree(&path, &target);
        } else {
            fs::copy(&path, &target).expect("copy file");
        }
    }
}

#[test]
fn a_repeated_fold_after_an_interrupted_removal_only_removes_the_rest() {
    let dir = temp_dir("fold-resume");
    let root = dir.path();
    write_bundle(root, COMMITTED_LOG, &[OLDER, NEWER]);
    fold_log(root, RELEASE, false).expect("first fold succeeds");
    let folded = read(root.join("log.md"));
    // The first fold wrote the log, then stopped before removing this fragment.
    fs::create_dir_all(root.join("log.d")).expect("recreate log.d");
    fs::write(root.join("log.d").join(OLDER.0), OLDER.1).expect("restore fragment");

    let summary = fold_log(root, RELEASE, false).expect("the repeated fold completes");

    assert_eq!(summary.folded, [OLDER.0]);
    assert_eq!(read(root.join("log.md")), folded, "nothing is filed twice");
    assert!(!root.join("log.d").exists());
}

#[test]
fn a_fold_into_an_existing_release_section_missing_the_entry_is_refused() {
    let dir = temp_dir("fold-existing");
    let root = dir.path();
    let log = "# Knowledge Bundle Log\n\n## 0.35.0\n\n### 2026-10-01, other, #1\n\n- Other.\n";
    write_bundle(root, log, &[NEWER]);
    let before = snapshot(root);

    let error = fold_log(root, RELEASE, false).expect_err("the entry is not in the section");

    assert!(
        error.to_string().contains("already has `## 0.35.0`"),
        "{error}"
    );
    assert_eq!(snapshot(root), before);
}

#[test]
fn a_migration_that_fails_part_way_removes_the_fragments_it_wrote() {
    let dir = temp_dir("migrate-partial");
    let root = dir.path();
    let log = concat!(
        "# Knowledge Bundle Log\n\n",
        "## Unreleased (2026-10-14, first, #1)\n\n- First.\n\n",
        "## Unreleased (2026-10-14, second, #2)\n\n- Second.\n",
    );
    write_bundle(root, log, &[]);
    // A read-only bundle root lets the fragments be written into the existing
    // log.d, then refuses the replacement log.
    fs::create_dir_all(root.join("log.d")).expect("create log.d");
    let original = fs::metadata(root).expect("root metadata").permissions();
    fs::set_permissions(root, fs::Permissions::from_mode(READ_ONLY_DIR_MODE))
        .expect("make the root read-only");

    let outcome = migrate_log(root, false);

    fs::set_permissions(root, original).expect("restore the root's mode");
    outcome.expect_err("the replacement log cannot be written");
    assert!(
        !root.join("log.d/1-first.md").exists(),
        "the first fragment is removed again"
    );
    assert!(
        !root.join("log.d/2-second.md").exists(),
        "the second fragment is removed again"
    );
    assert_eq!(
        read(root.join("log.md")),
        log,
        "the log keeps every section"
    );
}

#[test]
fn crlf_fragments_and_logs_parse_like_lf_ones() {
    let dir = temp_dir("crlf");
    let root = dir.path();
    let crlf = |text: &str| text.replace('\n', "\r\n");
    let log =
        crlf("# Knowledge Bundle Log\n\n## Unreleased (2026-10-14, crlf entry, #5)\n\n- Body.\n");
    write_bundle(root, &log, &[(NEWER.0, &crlf(NEWER.1))]);
    validate_docs(root).expect("a CRLF fragment validates");

    let summary = migrate_log(root, false).expect("a CRLF log migrates");

    assert_eq!(summary.written, ["5-crlf-entry.md"]);
    validate_docs(root).expect("the migrated bundle validates");
}

#[test]
fn an_impossible_date_and_an_indented_unreleased_heading_are_refused() {
    let dir = temp_dir("date");
    let root = dir.path();
    let impossible = (
        "7-impossible-date.md",
        "## Unreleased (2026-02-29, impossible date, #7)\n\n- Body.\n",
    );
    write_bundle(root, COMMITTED_LOG, &[impossible]);
    let error = validate_docs(root).expect_err("2026 has no 29 February");
    assert!(error.to_string().contains("date"), "{error}");

    let leap = (
        "8-leap-day.md",
        "## Unreleased (2028-02-29, leap day, #8)\n\n- Body.\n",
    );
    fs::remove_file(root.join("log.d").join(impossible.0)).expect("remove fragment");
    fs::write(root.join("log.d").join(leap.0), leap.1).expect("write leap fragment");
    validate_docs(root).expect("2028 has a 29 February");

    let indented =
        format!("{COMMITTED_LOG}\n   ## Unreleased (2026-10-14, indented, #9)\n\n- Body.\n");
    fs::write(root.join("log.md"), indented).expect("write log");
    let error = fold_log(root, RELEASE, true).expect_err("an indented heading is still a section");
    assert!(
        error.to_string().contains("belongs in a fragment"),
        "{error}"
    );
}

#[test]
fn a_resumed_fold_counts_only_entries_filed_under_its_own_release() {
    let dir = temp_dir("fold-resume-scope");
    let root = dir.path();
    // The entry sits under 0.34.2, and the 0.35.0 section lacks it.
    let log = concat!(
        "# Knowledge Bundle Log\n\n",
        "## 0.35.0\n\n### 2026-10-01, other, #1\n\n- Other.\n\n",
        "## 0.34.2\n\n### 2026-10-14, newer change, #200\n\n- Newer body.\n",
    );
    write_bundle(root, log, &[NEWER]);
    let before = snapshot(root);

    let error = fold_log(root, RELEASE, false).expect_err("the entry is not filed under 0.35.0");

    assert!(error.to_string().contains("is not filed in it"), "{error}");
    assert_eq!(snapshot(root), before, "the fragment is kept");
}

#[test]
fn docs_build_assembles_a_crlf_log_like_an_lf_one() {
    let dir = temp_dir("crlf-build");
    let source = dir.path().join("source");
    fs::create_dir_all(&source).expect("create source");
    write_bundle(
        &source,
        &COMMITTED_LOG.replace('\n', "\r\n"),
        &[(NEWER.0, &NEWER.1.replace('\n', "\r\n"))],
    );

    let summary = build_docs(BuildOptions {
        source_dir: source,
        output_root: dir.path().join("out"),
        pohunek_version: "0.0.0-test".to_owned(),
    })
    .expect("build succeeds");

    assert_eq!(
        read(summary.bundle_dir.join("log.md")),
        concat!(
            "# Knowledge Bundle Log\n\nIntroduction.\n\n",
            "## Unreleased (2026-10-14, newer change, #200)\n\n- Newer body.\n\n",
            "## 0.34.2\n\n- Earlier release.\n",
        )
    );
}

#[test]
fn a_fragment_body_with_an_indented_section_heading_is_refused() {
    let dir = temp_dir("indented-body");
    let root = dir.path();
    let indented = (
        "10-indented-heading.md",
        "## Unreleased (2026-10-14, indented heading, #10)\n\n- Body.\n\n  ## Stray section\n",
    );
    write_bundle(root, COMMITTED_LOG, &[indented]);

    let error = validate_docs(root).expect_err("the indented heading opens a section");

    assert!(error.to_string().contains("`##` heading"), "{error}");
}

#[test]
fn a_resumed_fold_ends_its_release_section_at_an_indented_heading() {
    let dir = temp_dir("fold-resume-indented");
    let root = dir.path();
    let log = concat!(
        "# Knowledge Bundle Log\n\n",
        "## 0.35.0\n\n### 2026-10-01, other, #1\n\n- Other.\n\n",
        "  ## 0.34.2\n\n### 2026-10-14, newer change, #200\n\n- Newer body.\n",
    );
    write_bundle(root, log, &[NEWER]);

    let error = fold_log(root, RELEASE, false).expect_err("the entry belongs to 0.34.2");

    assert!(error.to_string().contains("is not filed in it"), "{error}");
    assert!(root.join("log.d").join(NEWER.0).exists());
}
