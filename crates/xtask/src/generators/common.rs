//! Shared helpers for generated knowledge-reference concepts.

use std::fmt::Write as _;
use std::path::Path;

use crate::XtaskError;

/// Frontmatter values shared across generated concept files.
pub(crate) struct ConceptFrontmatter<'a> {
    pub concept_type: &'a str,
    pub id: &'a str,
    pub title: &'a str,
    pub description: &'a str,
    pub generated_from: &'a str,
    pub since: Option<&'a str>,
    pub tags: &'a [&'a str],
    pub intents: &'a [&'a str],
}

/// Build the YAML frontmatter block used by all generated reference concepts.
pub(crate) fn frontmatter(spec: &ConceptFrontmatter<'_>) -> String {
    let mut text = String::new();
    let _ = writeln!(text, "---");
    let _ = writeln!(text, "type: {}", spec.concept_type);
    let _ = writeln!(text, "id: {}", spec.id);
    let _ = writeln!(text, "title: \"{}\"", spec.title);
    let _ = writeln!(text, "description: \"{}\"", spec.description);
    let _ = writeln!(text, "source_kind: generated");
    let _ = writeln!(text, "generated_from: \"{}\"", spec.generated_from);

    if let Some(since) = spec.since {
        let _ = writeln!(text, "since: \"{since}\"");
    }

    let _ = writeln!(text, "tags:");
    for tag in spec.tags {
        let _ = writeln!(text, "  - {tag}");
    }
    let _ = writeln!(text, "intents:");
    for intent in spec.intents {
        let _ = writeln!(text, "  - {intent}");
    }
    let _ = writeln!(text, "---");
    text
}

/// Write a rendered concept file, creating parent directories as needed.
pub(crate) fn write_concept_file(path: &Path, content: &str) -> Result<(), XtaskError> {
    if let Some(parent) = path.parent() {
        crate::create_dir_all(parent)?;
    }
    std::fs::write(path, content).map_err(|source| XtaskError::Io {
        path: path.to_path_buf(),
        source,
    })
}
