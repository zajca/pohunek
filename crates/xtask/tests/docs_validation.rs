use std::path::PathBuf;

use knowledge::validate_bundle;
use pohunek_test_support::manifest_dir;
use xtask::{build_docs, BuildOptions, XtaskError};

fn source_fixture(name: &str) -> PathBuf {
    manifest_dir()
        .join("../knowledge/tests/fixtures")
        .join(name)
}

fn build_fixture(name: &str) -> Result<String, XtaskError> {
    let root = pohunek_test_support::tempdir_with_prefix("xtask-docs-validation-")
        .expect("private docs output root");
    build_docs(BuildOptions {
        source_dir: source_fixture(name),
        output_root: root.path().to_path_buf(),
        pohunek_version: "0.0.0-test".to_owned(),
    })
    .map(|summary| {
        let report = validate_bundle(&summary.bundle_dir).expect("built bundle validates");
        report
            .concepts
            .iter()
            .map(|concept| concept.id.as_str())
            .collect::<Vec<_>>()
            .join(",")
    })
}

fn rejected_fixture(name: &str, expected: &str) {
    let error = build_fixture(name).expect_err("invalid docs fixture must be rejected");
    assert!(
        matches!(error, XtaskError::BundleValidation(_)),
        "{name} returned unexpected error: {error}"
    );
    assert!(
        error.to_string().contains(expected),
        "{name} did not report {expected:?}: {error}"
    );
}

#[test]
fn docs_build_accepts_fixture_and_preserves_concept_metadata() {
    let concepts = build_fixture("good").expect("valid docs fixture builds");
    assert!(concepts.contains("guide-overview"));
    assert!(concepts.contains("runbook-setup"));
}

#[test]
fn docs_build_accepts_unknown_frontmatter_fields() {
    let concepts = build_fixture("unknown-field").expect("unknown fields remain compatible");
    assert!(concepts.contains("guide/unknown-field"));
}

#[test]
fn docs_build_rejects_missing_required_frontmatter_field() {
    rejected_fixture("missing-required", "missing field `description`");
}

#[test]
fn docs_build_rejects_unknown_concept_type() {
    rejected_fixture("bad-type", "BadType");
}

#[test]
fn docs_build_rejects_duplicate_concept_id() {
    rejected_fixture("duplicate-id", "duplicate concept id `shared-id`");
}

#[test]
fn docs_build_rejects_missing_since_on_behavioral_concept() {
    rejected_fixture(
        "missing-since",
        "concept `runbook-without-since` of type Runbook requires `since`",
    );
}

#[test]
fn docs_build_rejects_broken_relative_link() {
    rejected_fixture("broken-link", "missing.md");
}

#[test]
fn docs_build_rejects_missing_frontmatter() {
    rejected_fixture("missing-frontmatter", "missing frontmatter");
}

#[test]
fn docs_build_rejects_frontmatter_on_reserved_file() {
    rejected_fixture(
        "reserved-frontmatter",
        "must not contain concept frontmatter",
    );
}

#[test]
fn docs_build_rejects_absolute_markdown_link() {
    rejected_fixture("absolute-link", "/concepts/other.md");
}

#[test]
fn docs_build_rejects_parent_escape_markdown_link() {
    rejected_fixture("parent-escape-link", "../outside.md");
}

#[cfg(unix)]
#[test]
fn docs_build_rejects_symlinked_source_file() {
    let root = pohunek_test_support::tempdir_with_prefix("xtask-docs-symlink-")
        .expect("private docs source root");
    let source = root.path().join("source");
    std::fs::create_dir(&source).expect("create source directory");
    std::fs::write(source.join("index.md"), "# Index\n").expect("write index");
    std::os::unix::fs::symlink("index.md", source.join("linked-index.md"))
        .expect("create source symlink");

    let error = build_docs(BuildOptions {
        source_dir: source,
        output_root: root.path().join("output"),
        pohunek_version: "0.0.0-test".to_owned(),
    })
    .expect_err("symlinked source must be rejected");
    assert!(
        matches!(error, XtaskError::UnsupportedFileType(_)),
        "unexpected error: {error}"
    );
}
