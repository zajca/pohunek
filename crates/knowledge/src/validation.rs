//! Validation for markdown knowledge bundles.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

use crate::log::{self, FRAGMENT_DIR};
use crate::{Concept, ConceptType, PromptContract, CONCEPT_SCHEMA_VERSION, PROMPT_CONTRACT_PATH};

/// Summary returned after a bundle passes validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BundleValidationReport {
    /// Concept frontmatter schema version this bundle was checked against.
    pub schema_version: u32,
    /// Number of markdown files inspected during validation, not counting the
    /// log fragments.
    pub files_checked: usize,
    /// Number of unreleased log fragments under `log.d/` inspected.
    pub fragments_checked: usize,
    /// All concepts parsed from the bundle, in sorted file order.
    pub concepts: Vec<Concept>,
}

/// A failed bundle validation with one or more precise issues.
///
/// `Display` joins every contained [`BundleValidationIssue`] with `; `; that
/// loop cannot be expressed with a `thiserror` format string, so this type
/// keeps a hand-written `Display` and implements [`std::error::Error`]
/// directly. The per-issue messages it formats come from the `thiserror`
/// derive on [`BundleValidationIssue`].
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

impl std::error::Error for BundleValidationError {}

/// Individual validation failures.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum BundleValidationIssue {
    #[error("failed to read directory `{}`: {message}", path.display())]
    ReadDir {
        /// Directory that could not be read.
        path: PathBuf,
        /// Underlying I/O error message.
        message: String,
    },
    #[error("failed to read file `{}`: {message}", path.display())]
    ReadFile {
        /// File that could not be read.
        path: PathBuf,
        /// Underlying I/O error message.
        message: String,
    },
    #[error("missing frontmatter in `{}`", path.display())]
    MissingFrontmatter {
        /// File missing concept frontmatter.
        path: PathBuf,
    },
    #[error("invalid frontmatter in `{}`: {message}", path.display())]
    InvalidFrontmatter {
        /// File with malformed frontmatter.
        path: PathBuf,
        /// Parser error message.
        message: String,
    },
    #[error("duplicate concept id `{id}` in `{}` and `{}`", first_path.display(), duplicate_path.display())]
    DuplicateId {
        /// Concept id seen more than once.
        id: String,
        /// First file declaring the id.
        first_path: PathBuf,
        /// Later file re-declaring the id.
        duplicate_path: PathBuf,
    },
    #[error("concept `{id}` of type {concept_type:?} requires `since` in `{}`", path.display())]
    MissingSince {
        /// File whose concept is missing `since`.
        path: PathBuf,
        /// Concept id missing the `since` field.
        id: String,
        /// Concept type that requires `since`.
        concept_type: ConceptType,
    },
    #[error("broken internal markdown link `{target}` in `{}`", path.display())]
    BrokenLink {
        /// File containing the broken link.
        path: PathBuf,
        /// Link target that did not resolve.
        target: String,
    },
    #[error("reserved markdown file `{}` must not contain concept frontmatter", path.display())]
    ReservedFrontmatter {
        /// Reserved file carrying disallowed frontmatter.
        path: PathBuf,
    },
    #[error("invalid log fragment `{}`: {reason}", path.display())]
    LogFragment {
        /// Fragment file, or entry of the fragment directory, that was refused.
        path: PathBuf,
        /// The fragment rule it breaks.
        reason: String,
    },
    #[error("invalid prompt contract `{}`: {reason}", path.display())]
    PromptContract {
        /// The prompt contract file.
        path: PathBuf,
        /// The shape rule it breaks, with the offending line.
        reason: String,
    },
    #[error("unsupported file type in knowledge bundle: `{}`", path.display())]
    UnsupportedFileType {
        /// Path of the unsupported (non-file) entry.
        path: PathBuf,
    },
}

/// Validate a markdown knowledge bundle directory.
///
/// # Errors
///
/// Returns a [`BundleValidationError`] collecting every issue found: unreadable
/// directories or files, missing or invalid frontmatter, duplicate concept ids,
/// concepts missing a required `since`, broken internal links, reserved files
/// carrying frontmatter, an `assistant/system.md` that is not a well-formed
/// [`PromptContract`], log fragments that break the rules of
/// [`crate::log`], or unsupported file types.
pub fn validate_bundle(
    dir: impl AsRef<Path>,
) -> Result<BundleValidationReport, BundleValidationError> {
    let root = dir.as_ref();
    let normalized_root = normalize_path(root);
    let mut issues = Vec::new();
    let mut markdown_files = Vec::new();
    collect_markdown_files(root, &mut markdown_files, &mut issues);
    markdown_files.sort();
    let fragment_dir = root.join(FRAGMENT_DIR);
    let (fragments, markdown_files): (Vec<PathBuf>, Vec<PathBuf>) = markdown_files
        .into_iter()
        .partition(|path| path.starts_with(&fragment_dir));
    validate_fragments(&normalized_root, &fragment_dir, &fragments, &mut issues);

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

        if path.strip_prefix(root).ok() == Some(Path::new(PROMPT_CONTRACT_PATH)) {
            if let Err(error) = PromptContract::parse(&content) {
                issues.push(BundleValidationIssue::PromptContract {
                    path: path.clone(),
                    reason: error.to_string(),
                });
            }
        }

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

        let concept = match serde_saphyr::from_str::<Concept>(frontmatter) {
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
            fragments_checked: fragments.len(),
            concepts,
        })
    } else {
        Err(BundleValidationError::new(issues))
    }
}

/// Checks every unreleased log fragment and every entry of the fragment
/// directory. Links in a fragment resolve from the bundle root, where the
/// assembled log carries them.
fn validate_fragments(
    root: &Path,
    fragment_dir: &Path,
    fragments: &[PathBuf],
    issues: &mut Vec<BundleValidationIssue>,
) {
    if let Ok(entries) = fs::read_dir(fragment_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let is_markdown_file = entry.file_type().is_ok_and(|kind| kind.is_file())
                && path.extension().is_some_and(|extension| extension == "md");
            if !is_markdown_file {
                issues.push(BundleValidationIssue::LogFragment {
                    path,
                    reason: "only `.md` fragment files belong in log.d".to_owned(),
                });
            }
        }
    }
    let log_path = root.join(log::LOG_FILE);
    for path in fragments {
        if path.parent() != Some(fragment_dir) {
            continue;
        }
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
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if let Err(error) = log::parse_fragment(name, &content) {
            issues.push(BundleValidationIssue::LogFragment {
                path: path.clone(),
                reason: error.reason().to_owned(),
            });
        }
        validate_links_from(root, &log_path, path, &content, issues);
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
    validate_links_from(root, path, path, content, issues);
}

/// Checks the internal links of `content`, resolved as if it were the file
/// `base`, and reports a broken one against `path`.
fn validate_links_from(
    root: &Path,
    base: &Path,
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
            &base
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
