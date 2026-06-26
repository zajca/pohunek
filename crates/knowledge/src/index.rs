//! Read-only metadata accessors for the embedded knowledge bundle.

use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use include_dir::{Dir, DirEntry, File};
use serde::Serialize;

use crate::assistant::embedded_bundle;
use crate::{Concept, ConceptType, Deprecation, Intent};

/// Public concept metadata exposed by the embedded bundle index.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConceptMeta {
    #[serde(rename = "type")]
    pub r#type: ConceptType,
    pub id: String,
    pub title: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intents: Option<Vec<Intent>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed_in: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deprecated: Option<Deprecation>,
}

/// Error returned when embedded concept metadata cannot be indexed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BundleIndexError {
    path: PathBuf,
    message: String,
}

impl BundleIndexError {
    fn new(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }

    /// Bundle-relative path that failed to parse.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl fmt::Display for BundleIndexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid embedded knowledge concept `{}`: {}",
            self.path.display(),
            self.message
        )
    }
}

impl Error for BundleIndexError {}

impl From<Concept> for ConceptMeta {
    fn from(concept: Concept) -> Self {
        Self {
            r#type: concept.r#type,
            id: concept.id,
            title: concept.title,
            description: concept.description,
            intents: concept.intents,
            since: concept.since,
            changed_in: concept.changed_in,
            deprecated: concept.deprecated,
        }
    }
}

/// Return allowlisted metadata for concepts in the embedded knowledge bundle.
pub fn bundle_index() -> Result<Vec<ConceptMeta>, BundleIndexError> {
    let mut files = Vec::new();
    collect_markdown_files(embedded_bundle(), &mut files);

    let mut concepts: Vec<ConceptMeta> = files
        .into_iter()
        .filter(|file| !is_reserved_markdown(file))
        .map(concept_meta_from_file)
        .collect::<Result<Vec<_>, _>>()?;
    concepts.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(concepts)
}

fn concept_meta_from_file(file: &File<'_>) -> Result<ConceptMeta, BundleIndexError> {
    let content = file
        .contents_utf8()
        .ok_or_else(|| BundleIndexError::new(file.path(), "markdown content is not valid UTF-8"))?;
    parse_concept_meta(file.path(), content)
}

fn parse_concept_meta(
    path: impl Into<PathBuf>,
    content: &str,
) -> Result<ConceptMeta, BundleIndexError> {
    let path = path.into();
    let frontmatter = frontmatter(content)
        .ok_or_else(|| BundleIndexError::new(path.clone(), "missing frontmatter"))?;
    serde_yaml::from_str::<Concept>(frontmatter)
        .map(Into::into)
        .map_err(|err| BundleIndexError::new(path, format!("invalid frontmatter: {err}")))
}

fn collect_markdown_files<'a>(dir: &'a Dir<'a>, files: &mut Vec<&'a File<'a>>) {
    for entry in dir.entries() {
        match entry {
            DirEntry::Dir(dir) => collect_markdown_files(dir, files),
            DirEntry::File(file) if file.path().extension().is_some_and(|ext| ext == "md") => {
                files.push(file);
            }
            DirEntry::File(_) => {}
        }
    }
}

fn is_reserved_markdown(file: &File<'_>) -> bool {
    file.path()
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "index.md" | "log.md"))
}

fn frontmatter(content: &str) -> Option<&str> {
    let body = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))?;
    let delimiter = body.find("\n---\n").or_else(|| body.find("\r\n---\r\n"))?;
    Some(&body[..delimiter])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_concept_meta_reports_missing_frontmatter() {
        let error = parse_concept_meta("concepts/broken.md", "# Broken\n")
            .expect_err("missing frontmatter should be reported");

        assert_eq!(error.path(), std::path::Path::new("concepts/broken.md"));
        assert!(error.to_string().contains("missing frontmatter"));
    }

    #[test]
    fn parse_concept_meta_reports_invalid_schema() {
        let error = parse_concept_meta(
            "concepts/broken.md",
            "---\ntype: Unknown\nid: concept/broken\n---\n# Broken\n",
        )
        .expect_err("invalid schema should be reported");

        assert_eq!(error.path(), std::path::Path::new("concepts/broken.md"));
        assert!(error.to_string().contains("invalid frontmatter"));
    }
}
