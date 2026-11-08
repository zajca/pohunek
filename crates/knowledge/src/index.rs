//! Read-only metadata accessors for a knowledge bundle directory.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use thiserror::Error;

use crate::{Concept, ConceptType, Deprecation, Intent};

/// Public concept metadata exposed by the bundle index.
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

/// Error returned when concept metadata cannot be indexed.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
#[error("invalid knowledge concept `{}`: {message}", path.display())]
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

/// Most markdown files one bundle may hold.
const MAX_BUNDLE_FILES: usize = 4096;
/// Largest markdown file the index reads.
const MAX_CONCEPT_BYTES: u64 = 1024 * 1024;
/// Deepest directory level the index descends to.
const MAX_BUNDLE_DEPTH: usize = 16;

/// Return allowlisted metadata for concepts in the bundle directory `root`.
///
/// Symbolic links and other non-regular entries are skipped, so the index
/// never leaves `root`.
///
/// # Errors
///
/// Returns a [`BundleIndexError`] if the directory cannot be read, holds more
/// files or deeper levels than the bounds allow, or any concept file is too
/// large, not valid UTF-8, missing frontmatter, or has frontmatter that fails
/// to parse.
pub fn bundle_index(root: &Path) -> Result<Vec<ConceptMeta>, BundleIndexError> {
    let mut files = Vec::new();
    collect_markdown_files(root, Path::new(""), 0, &mut files)?;

    let mut concepts: Vec<ConceptMeta> = files
        .into_iter()
        .filter(|relative| !is_reserved_markdown(relative))
        .map(|relative| concept_meta_from_file(root, &relative))
        .collect::<Result<Vec<_>, _>>()?;
    concepts.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(concepts)
}

fn concept_meta_from_file(root: &Path, relative: &Path) -> Result<ConceptMeta, BundleIndexError> {
    let path = root.join(relative);
    let io_error = |error: std::io::Error| BundleIndexError::new(relative, error.to_string());
    let length = fs::metadata(&path).map_err(io_error)?.len();
    if length > MAX_CONCEPT_BYTES {
        return Err(BundleIndexError::new(
            relative,
            "markdown file is too large",
        ));
    }
    let content = fs::read(&path).map_err(io_error)?;
    let content = std::str::from_utf8(&content)
        .map_err(|_| BundleIndexError::new(relative, "markdown content is not valid UTF-8"))?;
    parse_concept_meta(relative, content)
}

fn parse_concept_meta(
    path: impl Into<PathBuf>,
    content: &str,
) -> Result<ConceptMeta, BundleIndexError> {
    let path = path.into();
    let frontmatter = frontmatter(content)
        .ok_or_else(|| BundleIndexError::new(path.clone(), "missing frontmatter"))?;
    serde_saphyr::from_str::<Concept>(frontmatter)
        .map(Into::into)
        .map_err(|err| BundleIndexError::new(path, format!("invalid frontmatter: {err}")))
}

fn collect_markdown_files(
    root: &Path,
    relative: &Path,
    depth: usize,
    files: &mut Vec<PathBuf>,
) -> Result<(), BundleIndexError> {
    if depth > MAX_BUNDLE_DEPTH {
        return Err(BundleIndexError::new(
            relative,
            "directory nesting is too deep",
        ));
    }
    let io_error = |error: std::io::Error| BundleIndexError::new(relative, error.to_string());
    for entry in fs::read_dir(root.join(relative)).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let child = relative.join(entry.file_name());
        let file_type = entry.file_type().map_err(io_error)?;
        if file_type.is_dir() {
            collect_markdown_files(root, &child, depth + 1, files)?;
        } else if file_type.is_file() && child.extension().is_some_and(|ext| ext == "md") {
            if files.len() >= MAX_BUNDLE_FILES {
                return Err(BundleIndexError::new(&child, "too many markdown files"));
            }
            files.push(child);
        }
    }
    Ok(())
}

/// Whether the bundle-relative `path` is not a concept: the reserved
/// `index.md` and `log.md`, or a log fragment, which a source bundle keeps
/// under `log.d/` and a built bundle carries only inside its assembled log.
fn is_reserved_markdown(path: &Path) -> bool {
    path.starts_with(crate::log::FRAGMENT_DIR)
        || path
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
