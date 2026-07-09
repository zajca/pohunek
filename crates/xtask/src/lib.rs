#![forbid(unsafe_code)]

mod checks;
mod eval;
mod generators;
mod site;
mod ts;

use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use knowledge::{validate_bundle, CONCEPT_SCHEMA_VERSION};
use serde::Serialize;
use sha2::{Digest, Sha256};

const MANUAL_SOURCE: &str = "docs/knowledge";
const GENERATED_REFERENCE: &str = "generated";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationSummary {
    pub schema_version: u32,
    pub files_checked: usize,
    pub concept_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildOptions {
    pub source_dir: PathBuf,
    pub output_root: PathBuf,
    pub pohunek_version: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildSummary {
    pub bundle_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub content_hash: String,
    pub files_copied: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SiteOptions {
    pub bundle_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub output_root: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SiteSummary {
    pub site_dir: PathBuf,
    pub offline_dir: PathBuf,
    pub pages_rendered: usize,
    pub pohunek_version: String,
}

#[derive(Debug)]
pub enum XtaskError {
    Usage(String),
    BundleValidation(knowledge::BundleValidationError),
    Io { path: PathBuf, source: io::Error },
    UnsupportedFileType(PathBuf),
    Json(serde_json::Error),
    InvalidPath(PathBuf),
}

impl fmt::Display for XtaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) => write!(f, "{message}"),
            Self::BundleValidation(error) => write!(f, "{error}"),
            Self::Io { path, source } => {
                write!(f, "failed to access `{}`: {source}", path.display())
            }
            Self::UnsupportedFileType(path) => {
                write!(f, "unsupported file type in `{}`", path.display())
            }
            Self::Json(error) => write!(f, "failed to serialize json: {error}"),
            Self::InvalidPath(path) => write!(
                f,
                "path `{}` cannot be represented as a deterministic relative path",
                path.display()
            ),
        }
    }
}

impl Error for XtaskError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::BundleValidation(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            Self::Json(error) => Some(error),
            Self::Usage(_) | Self::UnsupportedFileType(_) | Self::InvalidPath(_) => None,
        }
    }
}

#[derive(Serialize)]
struct Manifest<'a> {
    pohunek_version: &'a str,
    knowledge_schema_version: u32,
    reference: &'a str,
    sources: Vec<&'a str>,
    content_hash: &'a str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileEntry {
    source_path: PathBuf,
    relative_path: PathBuf,
}

pub fn validate_docs(source_dir: impl AsRef<Path>) -> Result<ValidationSummary, XtaskError> {
    let report = validate_bundle(source_dir).map_err(XtaskError::BundleValidation)?;
    Ok(ValidationSummary {
        schema_version: report.schema_version,
        files_checked: report.files_checked,
        concept_count: report.concepts.len(),
    })
}

pub fn build_docs(options: BuildOptions) -> Result<BuildSummary, XtaskError> {
    let BuildOptions {
        source_dir,
        output_root,
        pohunek_version,
    } = options;

    let bundle_dir = output_root.join("knowledge-bundle");
    if bundle_dir.exists() {
        remove_dir_all(&bundle_dir)?;
    }
    create_dir_all(&bundle_dir)?;

    let since = pohunek_version.as_str();

    // Phase 1: run all reference generators into the bundle dir.
    generators::cli::generate(&bundle_dir, since)?;
    generators::config::generate(&bundle_dir, since)?;
    generators::protocol::generate(&bundle_dir, since)?;
    generators::setup_assets::generate(&bundle_dir, since)?;

    // Phase 2: copy manual docs into the bundle dir.
    let manual_files = collect_files(&source_dir)?;
    for file in &manual_files {
        let destination = bundle_dir.join(&file.relative_path);
        if let Some(parent) = destination.parent() {
            create_dir_all(parent)?;
        }
        copy_file(&file.source_path, &destination)?;
    }

    // Phase 3: validate the merged bundle.
    validate_docs(&bundle_dir)?;

    // Phase 4: hash the merged bundle and write the manifest.
    let merged_files = collect_files(&bundle_dir)?;
    let content_hash = content_hash(&bundle_dir, &merged_files)?;
    let manifest_path = output_root.join("manifest.json");
    let manifest = Manifest {
        pohunek_version: &pohunek_version,
        knowledge_schema_version: CONCEPT_SCHEMA_VERSION,
        reference: GENERATED_REFERENCE,
        sources: vec!["cli", "config", "manual_docs", "protocol", "setup_assets"],
        content_hash: &content_hash,
    };
    write_manifest(&manifest_path, &manifest)?;

    Ok(BuildSummary {
        bundle_dir,
        manifest_path,
        content_hash,
        files_copied: merged_files.len(),
    })
}

/// Render the merged knowledge bundle to static HTML pages.
///
/// Reads the manifest for the `pohunek_version`, walks all `.md` files in
/// `bundle_dir` (in sorted order for determinism), converts each to HTML using
/// `pulldown-cmark`, and writes the result to both `site/` and `offline/`
/// under `output_root`. An `index.html` listing all pages is written to each
/// output directory. No external resources are used — all CSS is inlined.
pub fn build_site(options: SiteOptions) -> Result<SiteSummary, XtaskError> {
    site::build_site(options)
}

pub fn check_docs(
    source_dir: impl AsRef<Path>,
    output_root: impl AsRef<Path>,
) -> Result<bool, XtaskError> {
    checks::check_docs(source_dir, output_root)
}

pub fn run<I, S>(args: I) -> Result<(), XtaskError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut parsed_args = Vec::new();
    parsed_args.push(OsString::from("xtask"));
    parsed_args.extend(args.into_iter().map(Into::into));

    let command = XtaskCommand::try_parse_from(parsed_args)
        .map_err(|error| XtaskError::Usage(error.to_string()))?;

    let root = repo_root();
    match command.command {
        TopCommand::Eval => {
            if eval::run_eval() {
                Ok(())
            } else {
                Err(XtaskError::Usage(
                    "eval: one or more fixture commands failed to parse".to_string(),
                ))
            }
        }
        TopCommand::Docs { action } => match action {
            DocsAction::Validate => {
                let summary = validate_docs(root.join(MANUAL_SOURCE))?;
                println!(
                    "docs validate ok: {} files, {} concepts, schema v{}",
                    summary.files_checked, summary.concept_count, summary.schema_version
                );
                Ok(())
            }
            DocsAction::Build => {
                let summary = build_docs(BuildOptions {
                    source_dir: root.join(MANUAL_SOURCE),
                    output_root: root.join("target/pohunek-docs"),
                    pohunek_version: env!("CARGO_PKG_VERSION").to_string(),
                })?;
                println!(
                    "docs build ok: {} files, reference generated, hash {}",
                    summary.files_copied, summary.content_hash
                );
                Ok(())
            }
            DocsAction::Check => {
                let all_pass =
                    checks::check_docs(root.join(MANUAL_SOURCE), root.join("target/pohunek-docs"))?;
                if all_pass {
                    println!("docs check ok: all checks passed");
                    Ok(())
                } else {
                    Err(XtaskError::Usage(
                        "docs check failed: one or more checks did not pass".to_string(),
                    ))
                }
            }
            DocsAction::Site => {
                // Ensure the bundle is up-to-date before rendering the site.
                let build_summary = build_docs(BuildOptions {
                    source_dir: root.join(MANUAL_SOURCE),
                    output_root: root.join("target/pohunek-docs"),
                    pohunek_version: env!("CARGO_PKG_VERSION").to_string(),
                })?;
                let site_summary = build_site(SiteOptions {
                    bundle_dir: build_summary.bundle_dir,
                    manifest_path: build_summary.manifest_path,
                    output_root: root.join("target/pohunek-docs"),
                })?;
                println!(
                    "docs site ok: {} pages, site at {}, offline at {}",
                    site_summary.pages_rendered,
                    site_summary.site_dir.display(),
                    site_summary.offline_dir.display()
                );
                Ok(())
            }
        },
        TopCommand::Ts { action } => match action {
            TsAction::Generate => {
                let summary = ts::generate(&root)?;
                println!(
                    "ts generate ok: {} bindings, {} fixtures",
                    summary.generated_files, summary.fixture_files
                );
                Ok(())
            }
            TsAction::Check => {
                ts::check(&root)?;
                println!("ts check ok: generated TypeScript protocol files are current");
                Ok(())
            }
        },
    }
}

#[derive(Debug, Parser)]
#[command(name = "xtask", version, about)]
struct XtaskCommand {
    #[command(subcommand)]
    command: TopCommand,
}

#[derive(Debug, Subcommand)]
enum TopCommand {
    Eval,
    Docs {
        #[command(subcommand)]
        action: DocsAction,
    },
    Ts {
        #[command(subcommand)]
        action: TsAction,
    },
}

#[derive(Debug, Subcommand)]
enum DocsAction {
    Validate,
    Build,
    Check,
    Site,
}

#[derive(Debug, Subcommand)]
enum TsAction {
    Generate,
    Check,
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask crate lives under crates/xtask")
        .to_path_buf()
}

fn collect_files(root: &Path) -> Result<Vec<FileEntry>, XtaskError> {
    let mut files = Vec::new();
    collect_files_inner(root, root, &mut files)?;
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(files)
}

fn collect_files_inner(
    root: &Path,
    current: &Path,
    files: &mut Vec<FileEntry>,
) -> Result<(), XtaskError> {
    let mut entries = read_dir(current)?;
    entries.sort_by_key(std::fs::DirEntry::path);

    for entry in entries {
        let path = entry.path();
        let file_type = file_type(&path)?;
        if file_type.is_dir() {
            collect_files_inner(root, &path, files)?;
        } else if file_type.is_file() {
            let relative_path = path
                .strip_prefix(root)
                .ok()
                .ok_or_else(|| XtaskError::InvalidPath(path.clone()))?
                .to_path_buf();
            files.push(FileEntry {
                source_path: path,
                relative_path,
            });
        } else {
            return Err(XtaskError::UnsupportedFileType(path));
        }
    }

    Ok(())
}

fn content_hash(root: &Path, files: &[FileEntry]) -> Result<String, XtaskError> {
    let mut hasher = Sha256::new();
    for file in files {
        let relative_path = relative_path_string(&file.relative_path)?;
        let bytes = read_file(root.join(&file.relative_path))?;

        hasher.update(relative_path.as_bytes());
        hasher.update([0]);
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update([0]);
        hasher.update(bytes);
        hasher.update([0xff]);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn relative_path_string(path: &Path) -> Result<String, XtaskError> {
    let mut parts = Vec::new();
    for component in path.components() {
        let std::path::Component::Normal(part) = component else {
            return Err(XtaskError::InvalidPath(path.to_path_buf()));
        };
        let Some(part) = part.to_str() else {
            return Err(XtaskError::InvalidPath(path.to_path_buf()));
        };
        parts.push(part);
    }
    Ok(parts.join("/"))
}

fn write_manifest(path: &Path, manifest: &Manifest<'_>) -> Result<(), XtaskError> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }
    let mut json = serde_json::to_string_pretty(manifest).map_err(XtaskError::Json)?;
    json.push('\n');
    fs::write(path, json).map_err(|source| XtaskError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn read_dir(path: &Path) -> Result<Vec<fs::DirEntry>, XtaskError> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(path).map_err(|source| XtaskError::Io {
        path: path.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| XtaskError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        entries.push(entry);
    }
    Ok(entries)
}

fn file_type(path: &Path) -> Result<fs::FileType, XtaskError> {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type())
        .map_err(|source| XtaskError::Io {
            path: path.to_path_buf(),
            source,
        })
}

fn read_file(path: PathBuf) -> Result<Vec<u8>, XtaskError> {
    fs::read(&path).map_err(|source| XtaskError::Io { path, source })
}

fn copy_file(source: &Path, destination: &Path) -> Result<(), XtaskError> {
    fs::copy(source, destination)
        .map(|_| ())
        .map_err(|source_error| XtaskError::Io {
            path: destination.to_path_buf(),
            source: source_error,
        })
}

pub(crate) fn create_dir_all(path: &Path) -> Result<(), XtaskError> {
    fs::create_dir_all(path).map_err(|source| XtaskError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn remove_dir_all(path: &Path) -> Result<(), XtaskError> {
    fs::remove_dir_all(path).map_err(|source| XtaskError::Io {
        path: path.to_path_buf(),
        source,
    })
}
