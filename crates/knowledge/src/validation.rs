//! Validation for markdown knowledge bundles.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::{Concept, ConceptType, CONCEPT_SCHEMA_VERSION};

/// Summary returned after a bundle passes validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BundleValidationReport {
    pub schema_version: u32,
    pub files_checked: usize,
    pub concepts: Vec<Concept>,
}

/// A failed bundle validation with one or more precise issues.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BundleValidationError {
    issues: Vec<BundleValidationIssue>,
}

impl BundleValidationError {
    #[must_use]
    pub fn new(issues: Vec<BundleValidationIssue>) -> Self {
        Self { issues }
    }

    #[must_use]
    pub fn issues(&self) -> &[BundleValidationIssue] {
        &self.issues
    }
}

impl fmt::Display for BundleValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, issue) in self.issues.iter().enumerate() {
            if index > 0 {
                write!(f, "; ")?;
            }
            write!(f, "{issue}")?;
        }
        Ok(())
    }
}

impl Error for BundleValidationError {}

/// Individual validation failures.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BundleValidationIssue {
    ReadDir {
        path: PathBuf,
        message: String,
    },
    ReadFile {
        path: PathBuf,
        message: String,
    },
    MissingFrontmatter {
        path: PathBuf,
    },
    InvalidFrontmatter {
        path: PathBuf,
        message: String,
    },
    DuplicateId {
        id: String,
        first_path: PathBuf,
        duplicate_path: PathBuf,
    },
    MissingSince {
        path: PathBuf,
        id: String,
        concept_type: ConceptType,
    },
    BrokenLink {
        path: PathBuf,
        target: String,
    },
    ReservedFrontmatter {
        path: PathBuf,
    },
    UnsupportedFileType {
        path: PathBuf,
    },
}

impl fmt::Display for BundleValidationIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadDir { path, message } => {
                write!(
                    f,
                    "failed to read directory `{}`: {message}",
                    path.display()
                )
            }
            Self::ReadFile { path, message } => {
                write!(f, "failed to read file `{}`: {message}", path.display())
            }
            Self::MissingFrontmatter { path } => {
                write!(f, "missing frontmatter in `{}`", path.display())
            }
            Self::InvalidFrontmatter { path, message } => {
                write!(f, "invalid frontmatter in `{}`: {message}", path.display())
            }
            Self::DuplicateId {
                id,
                first_path,
                duplicate_path,
            } => write!(
                f,
                "duplicate concept id `{id}` in `{}` and `{}`",
                first_path.display(),
                duplicate_path.display()
            ),
            Self::MissingSince {
                path,
                id,
                concept_type,
            } => write!(
                f,
                "concept `{id}` of type {concept_type:?} requires `since` in `{}`",
                path.display()
            ),
            Self::BrokenLink { path, target } => write!(
                f,
                "broken internal markdown link `{target}` in `{}`",
                path.display()
            ),
            Self::ReservedFrontmatter { path } => write!(
                f,
                "reserved markdown file `{}` must not contain concept frontmatter",
                path.display()
            ),
            Self::UnsupportedFileType { path } => {
                write!(
                    f,
                    "unsupported file type in knowledge bundle: `{}`",
                    path.display()
                )
            }
        }
    }
}

/// Validate a markdown knowledge bundle directory.
pub fn validate_bundle(
    dir: impl AsRef<Path>,
) -> Result<BundleValidationReport, BundleValidationError> {
    let root = dir.as_ref();
    let normalized_root = normalize_path(root);
    let mut issues = Vec::new();
    let mut markdown_files = Vec::new();
    collect_markdown_files(root, &mut markdown_files, &mut issues);
    markdown_files.sort();

    let mut concepts = Vec::new();
    let mut ids: HashMap<String, PathBuf> = HashMap::new();

    for path in &markdown_files {
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) => {
                issues.push(BundleValidationIssue::ReadFile {
                    path: path.clone(),
                    message: error.to_string(),
                });
                continue;
            }
        };

        validate_links(&normalized_root, path, &content, &mut issues);

        if is_reserved_markdown(path) {
            if has_frontmatter(&content) {
                issues.push(BundleValidationIssue::ReservedFrontmatter { path: path.clone() });
            }
            continue;
        }

        let frontmatter = match frontmatter(&content) {
            Ok(frontmatter) => frontmatter,
            Err(issue) => {
                issues.push(issue.with_path(path.clone()));
                continue;
            }
        };

        let concept = match serde_yaml::from_str::<Concept>(frontmatter) {
            Ok(concept) => concept,
            Err(error) => {
                issues.push(BundleValidationIssue::InvalidFrontmatter {
                    path: path.clone(),
                    message: error.to_string(),
                });
                continue;
            }
        };

        if concept.r#type.requires_since() && concept.since.as_deref().unwrap_or("").is_empty() {
            issues.push(BundleValidationIssue::MissingSince {
                path: path.clone(),
                id: concept.id.clone(),
                concept_type: concept.r#type,
            });
        }

        if let Some(first_path) = ids.insert(concept.id.clone(), path.clone()) {
            issues.push(BundleValidationIssue::DuplicateId {
                id: concept.id.clone(),
                first_path,
                duplicate_path: path.clone(),
            });
        }

        concepts.push(concept);
    }

    if issues.is_empty() {
        Ok(BundleValidationReport {
            schema_version: CONCEPT_SCHEMA_VERSION,
            files_checked: markdown_files.len(),
            concepts,
        })
    } else {
        Err(BundleValidationError::new(issues))
    }
}

fn collect_markdown_files(
    dir: &Path,
    files: &mut Vec<PathBuf>,
    issues: &mut Vec<BundleValidationIssue>,
) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            issues.push(BundleValidationIssue::ReadDir {
                path: dir.to_path_buf(),
                message: error.to_string(),
            });
            return;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                issues.push(BundleValidationIssue::ReadDir {
                    path: dir.to_path_buf(),
                    message: error.to_string(),
                });
                continue;
            }
        };
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                issues.push(BundleValidationIssue::ReadFile {
                    path,
                    message: error.to_string(),
                });
                continue;
            }
        };

        if file_type.is_dir() {
            collect_markdown_files(&path, files, issues);
        } else if file_type.is_file() && path.extension().is_some_and(|extension| extension == "md")
        {
            files.push(path);
        } else if !file_type.is_file() {
            issues.push(BundleValidationIssue::UnsupportedFileType { path });
        }
    }
}

fn validate_links(
    root: &Path,
    path: &Path,
    content: &str,
    issues: &mut Vec<BundleValidationIssue>,
) {
    let mut remaining = content;
    while let Some(start) = remaining.find("](") {
        remaining = &remaining[start + 2..];
        let Some(end) = remaining.find(')') else {
            break;
        };
        let target = &remaining[..end];
        remaining = &remaining[end + 1..];

        let Some(target_path) = markdown_link_target(target) else {
            continue;
        };

        if target_path.is_absolute() {
            issues.push(BundleValidationIssue::BrokenLink {
                path: path.to_path_buf(),
                target: target.to_string(),
            });
            continue;
        }

        let resolved = normalize_path(
            &path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(target_path),
        );
        if !is_under_root(root, &resolved) || !resolved.is_file() {
            issues.push(BundleValidationIssue::BrokenLink {
                path: path.to_path_buf(),
                target: target.to_string(),
            });
        }
    }
}

fn markdown_link_target(target: &str) -> Option<&Path> {
    if target.is_empty() || target.starts_with('#') {
        return None;
    }
    if target.contains("://") || target.starts_with("mailto:") {
        return None;
    }
    let target_path = target.split_once('#').map_or(target, |(path, _)| path);
    Path::new(target_path)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
        .then(|| Path::new(target_path))
}

fn is_under_root(root: &Path, path: &Path) -> bool {
    path.starts_with(root)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
            Component::RootDir | Component::Prefix(_) => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn is_reserved_markdown(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "index.md" | "log.md"))
}

fn has_frontmatter(content: &str) -> bool {
    content.starts_with("---\n") || content.starts_with("---\r\n")
}

fn frontmatter(content: &str) -> Result<&str, FrontmatterIssue> {
    let body = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
        .ok_or(FrontmatterIssue::Missing)?;
    let delimiter = body
        .find("\n---\n")
        .or_else(|| body.find("\r\n---\r\n"))
        .ok_or_else(|| FrontmatterIssue::Invalid("missing closing `---` delimiter".to_string()))?;
    Ok(&body[..delimiter])
}

enum FrontmatterIssue {
    Missing,
    Invalid(String),
}

impl FrontmatterIssue {
    fn with_path(self, path: PathBuf) -> BundleValidationIssue {
        match self {
            Self::Missing => BundleValidationIssue::MissingFrontmatter { path },
            Self::Invalid(message) => BundleValidationIssue::InvalidFrontmatter { path, message },
        }
    }
}
