use std::path::PathBuf;

use knowledge::{
    validate_bundle, BundleValidationError, BundleValidationIssue, BundleValidationReport,
    ConceptType, CONCEPT_SCHEMA_VERSION,
};
use pohunek_test_support::manifest_dir;
use xtask::{build_docs, validate_docs, BuildOptions, XtaskError};

fn source_fixture(name: &str) -> PathBuf {
    manifest_dir()
        .join("../knowledge/tests/fixtures")
        .join(name)
}

/// Builds the fixture through `docs build` and re-reads the merged bundle it wrote.
///
/// The merged bundle also holds the generated CLI, config, and protocol
/// references, so exact fixture counts come from `docs validate` instead.
fn build_fixture(name: &str) -> Result<BundleValidationReport, XtaskError> {
    let root = pohunek_test_support::tempdir_with_prefix("xtask-docs-validation-")
        .expect("private docs output root");
    build_docs(BuildOptions {
        source_dir: source_fixture(name),
        output_root: root.path().to_path_buf(),
        pohunek_version: "0.0.0-test".to_owned(),
    })
    .map(|summary| validate_bundle(&summary.bundle_dir).expect("built bundle validates"))
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
        BundleValidationIssue::LogFragment { .. } => "log_fragment",
        BundleValidationIssue::PromptContract { .. } => "prompt_contract",
        BundleValidationIssue::UnsupportedFileType { .. } => "unsupported_file_type",
    }
}

fn assert_bundle_issue(name: &str, error: &XtaskError, kind: &str, expected: &str) {
    let XtaskError::BundleValidation(validation) = error else {
        panic!("{name} returned unexpected error: {error}");
    };
    assert!(
        has_issue(validation, kind),
        "{name} did not report a {kind} issue: {error}"
    );
    assert!(
        error.to_string().contains(expected),
        "{name} did not report {expected:?}: {error}"
    );
}

fn has_issue(error: &BundleValidationError, kind: &str) -> bool {
    error.issues().iter().any(|issue| issue_kind(issue) == kind)
}

fn rejected_fixture(name: &str, kind: &str, expected: &str) {
    let error = build_fixture(name).expect_err("invalid docs fixture must be rejected");
    assert_bundle_issue(name, &error, kind, expected);
}

#[test]
fn docs_validate_counts_fixture_files_and_concepts() {
    let summary = validate_docs(source_fixture("good")).expect("valid docs fixture validates");

    assert_eq!(summary.schema_version, CONCEPT_SCHEMA_VERSION);
    assert_eq!(summary.files_checked, 4);
    assert_eq!(summary.concept_count, 2);
}

#[test]
fn docs_build_accepts_fixture_and_preserves_concept_metadata() {
    let report = build_fixture("good").expect("valid docs fixture builds");

    assert_eq!(report.schema_version, CONCEPT_SCHEMA_VERSION);
    assert!(report
        .concepts
        .iter()
        .any(|concept| concept.id == "guide-overview" && concept.r#type == ConceptType::Guide));
    let setup = report
        .concepts
        .iter()
        .find(|concept| concept.id == "runbook-setup")
        .expect("setup runbook exists");
    assert_eq!(setup.r#type, ConceptType::Runbook);
    let expected = vec!["0.3.4".to_owned()];
    assert_eq!(setup.changed_in.as_ref(), Some(&expected));
}

#[test]
fn docs_build_accepts_unknown_frontmatter_fields() {
    let summary =
        validate_docs(source_fixture("unknown-field")).expect("unknown fields remain compatible");
    assert_eq!(summary.concept_count, 1);

    let report = build_fixture("unknown-field").expect("unknown fields remain compatible");
    assert!(report
        .concepts
        .iter()
        .any(|concept| concept.id == "guide/unknown-field"));
}

#[test]
fn docs_build_rejects_missing_required_frontmatter_field() {
    rejected_fixture(
        "missing-required",
        "invalid_frontmatter",
        "missing field `description`",
    );
}

#[test]
fn docs_build_rejects_unknown_concept_type() {
    rejected_fixture("bad-type", "invalid_frontmatter", "BadType");
}

#[test]
fn docs_build_rejects_duplicate_concept_id() {
    rejected_fixture(
        "duplicate-id",
        "duplicate_id",
        "duplicate concept id `shared-id`",
    );
}

#[test]
fn docs_build_rejects_missing_since_on_behavioral_concept() {
    rejected_fixture(
        "missing-since",
        "missing_since",
        "concept `runbook-without-since` of type Runbook requires `since`",
    );
}

#[test]
fn docs_build_rejects_broken_relative_link() {
    rejected_fixture("broken-link", "broken_link", "missing.md");
}

#[test]
fn docs_build_rejects_missing_frontmatter() {
    rejected_fixture(
        "missing-frontmatter",
        "missing_frontmatter",
        "missing frontmatter",
    );
}

#[test]
fn docs_build_rejects_frontmatter_on_reserved_file() {
    rejected_fixture(
        "reserved-frontmatter",
        "reserved_frontmatter",
        "must not contain concept frontmatter",
    );
}

#[test]
fn docs_build_rejects_absolute_markdown_link() {
    rejected_fixture("absolute-link", "broken_link", "/concepts/other.md");
}

#[test]
fn docs_build_rejects_parent_escape_markdown_link() {
    rejected_fixture("parent-escape-link", "broken_link", "../outside.md");
}

#[test]
fn docs_check_accepts_well_formed_prompt_contract() {
    let summary = validate_docs(source_fixture("prompt-contract-good"))
        .expect("a well-formed prompt contract validates");
    assert_eq!(summary.concept_count, 1);
}

#[test]
fn prompt_contract_joins_continuation_lines_into_one_item() {
    let text =
        std::fs::read_to_string(source_fixture("prompt-contract-good").join("assistant/system.md"))
            .expect("read the fixture contract");
    let contract = knowledge::PromptContract::parse(&text).expect("well-formed contract");
    assert_eq!(
        contract.safety,
        ["Never print secrets. Continue on an indented line. And on a tab-indented line."]
    );
    assert_eq!(contract.navigation, ["Start at the index."]);
}

#[test]
fn docs_check_rejects_prose_in_prompt_contract() {
    let error = validate_docs(source_fixture("prompt-contract-prose"))
        .expect_err("prose in the contract must be rejected");
    assert_bundle_issue(
        "prompt-contract-prose",
        &error,
        "prompt_contract",
        "line 15: `Safety` holds a line that is not a `- ` or `N. ` list item",
    );
}

#[test]
fn docs_check_rejects_nested_list_in_prompt_contract() {
    let error = validate_docs(source_fixture("prompt-contract-nested"))
        .expect_err("a nested list in the contract must be rejected");
    assert_bundle_issue(
        "prompt-contract-nested",
        &error,
        "prompt_contract",
        "line 15: `Safety` holds a nested list or heading",
    );
}

#[test]
fn docs_check_rejects_tab_nested_list_in_prompt_contract() {
    let error = validate_docs(source_fixture("prompt-contract-tab-nested"))
        .expect_err("a tab-nested list in the contract must be rejected");
    assert_bundle_issue(
        "prompt-contract-tab-nested",
        &error,
        "prompt_contract",
        "line 15: `Safety` holds a nested list or heading",
    );
}

#[test]
fn docs_check_rejects_unicode_space_nested_list_in_prompt_contract() {
    let error = validate_docs(source_fixture("prompt-contract-nbsp-nested"))
        .expect_err("a list nested by a no-break space must be rejected");
    assert_bundle_issue(
        "prompt-contract-nbsp-nested",
        &error,
        "prompt_contract",
        "line 15: `Safety` holds a nested list or heading",
    );
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

// `docs build` rejects a symlink while copying sources, before the knowledge
// validator runs; `docs validate` hands the source tree to the validator directly.
#[cfg(unix)]
#[test]
fn docs_validate_rejects_symlinked_source_file() {
    let root = pohunek_test_support::tempdir_with_prefix("xtask-docs-validate-symlink-")
        .expect("private docs source root");
    std::fs::write(root.path().join("index.md"), "# Index\n").expect("write index");
    std::os::unix::fs::symlink("index.md", root.path().join("linked-index.md"))
        .expect("create source symlink");

    let error = validate_docs(root.path()).expect_err("symlinked source must be rejected");
    assert_bundle_issue(
        "symlinked source",
        &error,
        "unsupported_file_type",
        "linked-index.md",
    );
}
