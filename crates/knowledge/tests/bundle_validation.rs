use std::path::{Path, PathBuf};

use knowledge::{
    validate_bundle, BundleValidationError, BundleValidationIssue, ConceptType,
    CONCEPT_SCHEMA_VERSION,
};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn repo_knowledge_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("docs")
        .join("knowledge")
}

fn issue_kinds(error: &BundleValidationError) -> Vec<&'static str> {
    error.issues().iter().map(issue_kind).collect()
}

fn issue_kind(issue: &BundleValidationIssue) -> &'static str {
    match issue {
        BundleValidationIssue::ReadDir { .. } => "read_dir",
        BundleValidationIssue::ReadFile { .. } => "read_file",
        BundleValidationIssue::MissingFrontmatter { .. } => "missing_frontmatter",
        BundleValidationIssue::InvalidFrontmatter { .. } => "invalid_frontmatter",
        BundleValidationIssue::DuplicateId { .. } => "duplicate_id",
        BundleValidationIssue::MissingSince { .. } => "missing_since",
        BundleValidationIssue::BrokenLink { .. } => "broken_link",
        BundleValidationIssue::ReservedFrontmatter { .. } => "reserved_frontmatter",
        BundleValidationIssue::UnsupportedFileType { .. } => "unsupported_file_type",
    }
}

fn has_issue(error: &BundleValidationError, expected: &str) -> bool {
    issue_kinds(error).contains(&expected)
}

#[test]
fn good_bundle_passes_validation() {
    let report = validate_bundle(fixture("good")).expect("fixture should validate");

    assert_eq!(report.schema_version, CONCEPT_SCHEMA_VERSION);
    assert_eq!(report.concepts.len(), 2);
    assert_eq!(report.files_checked, 4);
    assert!(report
        .concepts
        .iter()
        .any(|concept| concept.id == "guide-overview" && concept.r#type == ConceptType::Guide));
    assert!(report
        .concepts
        .iter()
        .any(|concept| concept.id == "runbook-setup" && concept.r#type == ConceptType::Runbook));
    let setup = report
        .concepts
        .iter()
        .find(|concept| concept.id == "runbook-setup")
        .expect("setup runbook exists");
    let expected = vec!["0.3.4".to_owned()];
    assert_eq!(setup.changed_in.as_ref(), Some(&expected));
}

#[test]
fn committed_docs_knowledge_bundle_passes_validation() {
    let report = validate_bundle(repo_knowledge_dir()).expect("docs/knowledge should validate");

    assert_eq!(report.schema_version, CONCEPT_SCHEMA_VERSION);
    assert_eq!(report.files_checked, 21);
    assert_eq!(report.concepts.len(), 19);
}

#[test]
fn missing_required_field_fails_validation() {
    let error = validate_bundle(fixture("missing-required")).expect_err("fixture should fail");

    assert!(has_issue(&error, "invalid_frontmatter"));
    assert!(error.to_string().contains("missing field `description`"));
}

#[test]
fn bad_type_fails_validation() {
    let error = validate_bundle(fixture("bad-type")).expect_err("fixture should fail");

    assert!(has_issue(&error, "invalid_frontmatter"));
    assert!(error.to_string().contains("BadType"));
}

#[test]
fn unknown_frontmatter_field_is_tolerated_when_reading() {
    let report = validate_bundle(fixture("unknown-field")).expect("fixture should validate");

    assert_eq!(report.concepts.len(), 1);
    assert_eq!(report.concepts[0].id, "guide/unknown-field");
}

#[test]
fn duplicate_id_fails_validation() {
    let error = validate_bundle(fixture("duplicate-id")).expect_err("fixture should fail");

    assert!(has_issue(&error, "duplicate_id"));
    assert!(error
        .to_string()
        .contains("duplicate concept id `shared-id`"));
}

#[test]
fn behavior_bearing_concept_requires_since() {
    let error = validate_bundle(fixture("missing-since")).expect_err("fixture should fail");

    assert!(has_issue(&error, "missing_since"));
    assert!(error
        .to_string()
        .contains("concept `runbook-without-since` of type Runbook requires `since`"));
}

#[test]
fn broken_internal_relative_link_fails_validation() {
    let error = validate_bundle(fixture("broken-link")).expect_err("fixture should fail");

    assert!(has_issue(&error, "broken_link"));
    assert!(error.to_string().contains("missing.md"));
}

#[test]
fn missing_frontmatter_fails_validation() {
    let error = validate_bundle(fixture("missing-frontmatter")).expect_err("fixture should fail");

    assert!(has_issue(&error, "missing_frontmatter"));
}

#[test]
fn reserved_frontmatter_fails_validation() {
    let error = validate_bundle(fixture("reserved-frontmatter")).expect_err("fixture should fail");

    assert!(has_issue(&error, "reserved_frontmatter"));
}

#[test]
fn absolute_markdown_link_fails_validation() {
    let error = validate_bundle(fixture("absolute-link")).expect_err("fixture should fail");

    assert!(has_issue(&error, "broken_link"));
    assert!(error.to_string().contains("/concepts/other.md"));
}

#[test]
fn parent_escape_markdown_link_fails_validation() {
    let error = validate_bundle(fixture("parent-escape-link")).expect_err("fixture should fail");

    assert!(has_issue(&error, "broken_link"));
    assert!(error.to_string().contains("../outside.md"));
}

#[cfg(unix)]
#[test]
fn unsupported_file_type_fails_validation() {
    let root = temp_dir("unsupported-file-type");
    std::fs::write(root.join("index.md"), "# Index\n").expect("write index");
    std::os::unix::fs::symlink("index.md", root.join("linked-index.md")).expect("create symlink");

    let error = validate_bundle(&root).expect_err("fixture should fail");

    assert!(has_issue(&error, "unsupported_file_type"));
}

#[cfg(unix)]
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pohunek-knowledge-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after unix epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}
