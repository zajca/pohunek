#![forbid(unsafe_code)]

mod affected;
mod agent_skill;
mod attestation;
mod catalog;
mod catalog_key;
mod checks;
mod doc_drift;
mod eval;
mod generators;
mod hermes;
mod hermes_mock;
mod hermes_skill;
mod knowledge_log;
pub use knowledge_log::{fold as fold_log, migrate as migrate_log, FoldSummary, MigrateSummary};
mod package_affected;
mod package_baseline;
mod package_bump;
mod package_evidence;
mod release;
mod release_inventory;
mod release_policy;
#[cfg(test)]
mod release_tests;
mod release_tree;
mod runtime_package;
mod site;
pub mod test_code;
mod test_runtime;
mod throwaway_trust;
mod ts;
mod upstream_stage;
#[cfg(test)]
mod upstream_stage_tests;

use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

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
    /// A knowledge-log fragment or the log itself was refused.
    KnowledgeLog(knowledge::log::LogError),
    Io {
        path: PathBuf,
        source: io::Error,
    },
    UnsupportedFileType(PathBuf),
    Json(serde_json::Error),
    Yaml(serde_saphyr::SerializeError),
    Package(::package::ArchiveError),
    /// A compatibility attestation input was refused.
    Attestation(attestation::AttestationError),
    /// An upstream stage input or staged tree was refused.
    UpstreamStage(upstream_stage::StageError),
    /// A release input was refused by the assembler or the inventory check.
    Release(release::ReleaseError),
    /// A catalog could not be built, signed or verified.
    Catalog(::package::CatalogError),
    /// A trust anchor could not be built or read.
    Anchor(::package::AnchorFileError),
    /// A key file was refused; never carries key material.
    KeyFile {
        path: PathBuf,
        fault: catalog_key::KeyFileFault,
    },
    /// The archive output path lies inside the package directory it packs.
    OutputInsideInput(PathBuf),
    /// The archive output path is a symbolic link.
    OutputIsSymlink(PathBuf),
    InvalidPath(PathBuf),
    /// A delegated command ran and exited unsuccessfully.
    ChildExit {
        command: String,
        status: ExitStatus,
    },
}

/// Process exit code for xtask failures that are not a delegated command's own exit.
const FAILURE_EXIT_CODE: i32 = 2;

/// Shell convention for a child killed by a signal: 128 plus the signal number.
const SIGNAL_EXIT_BASE: i32 = 128;

impl XtaskError {
    /// Process exit code that reports this error.
    ///
    /// A failed delegated command propagates its own exit code, so callers such
    /// as bacon and CI see the test runner's status; every other error exits 2.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::ChildExit { status, .. } => status.code().unwrap_or_else(|| {
                use std::os::unix::process::ExitStatusExt as _;
                SIGNAL_EXIT_BASE + status.signal().unwrap_or(0)
            }),
            _ => FAILURE_EXIT_CODE,
        }
    }
}

impl fmt::Display for XtaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) => write!(f, "{message}"),
            Self::BundleValidation(error) => write!(f, "{error}"),
            Self::KnowledgeLog(error) => write!(f, "knowledge log: {error}"),
            Self::Io { path, source } => {
                write!(f, "failed to access `{}`: {source}", path.display())
            }
            Self::UnsupportedFileType(path) => {
                write!(f, "unsupported file type in `{}`", path.display())
            }
            Self::Json(error) => write!(f, "failed to serialize json: {error}"),
            Self::Yaml(error) => write!(f, "failed to serialize yaml: {error}"),
            Self::Package(error) => write!(f, "package archive: {error}"),
            Self::Attestation(error) => write!(f, "attestation: {error}"),
            Self::UpstreamStage(error) => write!(f, "upstream stage: {error}"),
            Self::Release(error) => write!(f, "release: {error}"),
            Self::Catalog(error) => write!(f, "catalog: {error}"),
            Self::Anchor(error) => write!(f, "trust anchor: {error}"),
            Self::KeyFile { path, fault } => {
                write!(f, "key file `{}` was refused: {fault}", path.display())
            }
            Self::OutputIsSymlink(path) => {
                write!(f, "output `{}` is a symbolic link", path.display())
            }
            Self::OutputInsideInput(path) => write!(
                f,
                "archive output `{}` is inside the package directory",
                path.display()
            ),
            Self::InvalidPath(path) => write!(
                f,
                "path `{}` cannot be represented as a deterministic relative path",
                path.display()
            ),
            Self::ChildExit { command, status } => write!(f, "`{command}` failed with {status}"),
        }
    }
}

impl Error for XtaskError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::BundleValidation(error) => Some(error),
            Self::KnowledgeLog(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            Self::Json(error) => Some(error),
            Self::Yaml(error) => Some(error),
            Self::Package(error) => Some(error),
            Self::Attestation(error) => Some(error),
            Self::UpstreamStage(error) => Some(error),
            Self::Release(error) => Some(error),
            Self::Catalog(error) => Some(error),
            Self::Anchor(error) => Some(error),
            Self::Usage(_)
            | Self::KeyFile { .. }
            | Self::UnsupportedFileType(_)
            | Self::InvalidPath(_)
            | Self::OutputInsideInput(_)
            | Self::OutputIsSymlink(_)
            | Self::ChildExit { .. } => None,
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

    // Phase 2: copy manual docs into the bundle dir. The unreleased log
    // fragments reach the bundle inside the assembled log only.
    let manual_files = collect_files(&source_dir)?;
    for file in &manual_files {
        if knowledge_log::is_fragment_path(&file.relative_path) {
            continue;
        }
        let destination = bundle_dir.join(&file.relative_path);
        if let Some(parent) = destination.parent() {
            create_dir_all(parent)?;
        }
        copy_file(&file.source_path, &destination)?;
    }
    knowledge_log::assemble_into(&source_dir, &bundle_dir)?;

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
        sources: vec!["cli", "config", "manual_docs", "protocol"],
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

/// Runs one `docs log` action against the source bundle `source_dir`.
fn run_log(source_dir: &Path, action: LogAction) -> Result<(), XtaskError> {
    match action {
        LogAction::Fold { version, dry_run } => {
            let summary = fold_log(source_dir, &version, dry_run)?;
            let verb = if dry_run { "would fold" } else { "folded" };
            println!(
                "docs log fold: {verb} {} fragment(s) under {version}",
                summary.folded.len()
            );
            for name in &summary.folded {
                println!("  {name}");
            }
        }
        LogAction::Migrate { dry_run } => {
            let summary = migrate_log(source_dir, dry_run)?;
            let verb = if dry_run { "would write" } else { "wrote" };
            println!(
                "docs log migrate: {verb} {} fragment(s)",
                summary.written.len()
            );
            for name in &summary.written {
                println!("  {name}");
            }
        }
    }
    Ok(())
}

pub fn check_docs(
    source_dir: impl AsRef<Path>,
    output_root: impl AsRef<Path>,
) -> Result<bool, XtaskError> {
    checks::check_docs(source_dir, output_root)
}

/// Runs the drift checks between the docs and their sources of truth (the
/// clap command tree, the workspace members, `runtime-packages/`, the
/// documentation indexes) against the checkout at `repo`, printing one
/// `[PASS]`/`[FAIL]` line per check, and returns the names of the failed ones.
///
/// # Errors
///
/// Returns [`XtaskError::Io`] when a page cannot be read, and
/// [`XtaskError::UnsupportedFileType`] for a symbolic link where a documentation
/// root is expected.
pub fn check_doc_drift(repo: impl AsRef<Path>) -> Result<Vec<&'static str>, XtaskError> {
    doc_drift::check(repo.as_ref())
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
            DocsAction::Log { action } => run_log(&root.join(MANUAL_SOURCE), action),
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
        TopCommand::Hermes { action } => run_hermes(action, &root),
        TopCommand::Affected(options) => affected::run(&root, &options),
        TopCommand::Package { action } => run_package(action),
        TopCommand::Catalog { action } => run_catalog(action),
        TopCommand::Compat { action } => attestation::run(action, &root),
        TopCommand::Release { action } => release::run(action, &root),
        TopCommand::AgentSkill { action } => match action {
            AgentSkillAction::Generate => {
                agent_skill::generate(&root)?;
                println!("agent-skill generate ok: checked skill artifact updated");
                Ok(())
            }
        },
    }
}

fn run_hermes(action: HermesAction, root: &Path) -> Result<(), XtaskError> {
    match action {
        HermesAction::Compatibility {
            hermes_bin,
            bin_dir,
            package,
        } => {
            let summary = hermes::compatibility(root, &hermes_bin, &bin_dir, &package)?;
            let message = hermes_compatibility_message(&summary);
            println!("{message}");
            Ok(())
        }
        HermesAction::ThrowawayCatalog {
            package,
            output_dir,
        } => {
            hermes::write_throwaway_trust(&package, &output_dir)?;
            println!(
                "hermes throwaway-catalog ok: {} and {} written to {}",
                package::ANCHOR_FILE_NAME,
                hermes::CATALOG_FILE,
                output_dir.display()
            );
            Ok(())
        }
        HermesAction::RefreshGoldens { hermes_bin } => {
            let summary = hermes::refresh_goldens(root, &hermes_bin)?;
            println!(
                "hermes golden refresh ok: {} captures, {} unsupported diagnostics, manifest {}",
                summary.captures,
                summary.unsupported,
                summary.manifest_path.display()
            );
            Ok(())
        }
        HermesAction::GenerateSkill => {
            hermes_skill::generate(root)?;
            println!("hermes generate-skill ok: checked skill artifact updated");
            Ok(())
        }
        HermesAction::CheckSkill => {
            if hermes_skill::check(root)? {
                println!("hermes check-skill ok: checked skill artifact is current");
                Ok(())
            } else {
                Err(XtaskError::Usage(
                    "hermes check-skill failed: generated skill is missing or stale".to_string(),
                ))
            }
        }
    }
}

fn hermes_compatibility_message(summary: &hermes::CompatibilitySummary) -> String {
    format!(
        "hermes compatibility ok: {} {}, {} CLI checks, {} plugin checks, {} golden records",
        summary.release,
        summary.tag,
        summary.cli_checks,
        summary.plugin_checks,
        summary.golden_records
    )
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
    Hermes {
        #[command(subcommand)]
        action: HermesAction,
    },
    AgentSkill {
        #[command(subcommand)]
        action: AgentSkillAction,
    },
    /// Run the fast tests of packages affected by the current change.
    ///
    /// Selects the packages owning changed, staged, unstaged, and untracked
    /// files plus their dependents, and escalates to every test for
    /// workspace-wide or unmapped paths. Never a substitute for the full gates.
    Affected(affected::Options),
    /// Build, sign and verify the runtime catalog and its trust anchor.
    ///
    /// A signing key is read only from a file named by `--key-file`: it must
    /// be a regular file owned by the caller with mode 0600 or stricter and
    /// holds the 32-byte seed as 64 lowercase hex characters. Key material
    /// is never read from arguments or the environment and never printed.
    Catalog {
        #[command(subcommand)]
        action: CatalogAction,
    },
    /// Attest, verify and matrix-check runtime package compatibility.
    ///
    /// Every digest is recomputed from file bytes; a report or attestation
    /// only supplies values that must equal the recomputed ones.
    Compat {
        #[command(subcommand)]
        action: attestation::CompatAction,
    },
    /// Assemble and verify the release bundle.
    ///
    /// `assemble` recomputes every digest from the producer artifacts and
    /// writes the signed catalog, the final daemon archives and the
    /// inventory only when everything verified; `verify-inventory` checks a
    /// bundle directory against its inventory, the trust anchor named by
    /// `--anchor` and the catalog signed under it. A signing key is
    /// read only from the file named by `--key-file`.
    Release {
        #[command(subcommand)]
        action: release::ReleaseAction,
    },
    /// Build and check canonical runtime package archives.
    Package {
        #[command(subcommand)]
        action: PackageAction,
    },
}

#[derive(Debug, Subcommand)]
enum CatalogAction {
    /// Build the unsigned catalog body from a package spec.
    ///
    /// The spec is JSON: `schema_version` 1, `sequence`, `expires_at`,
    /// `revoked_key_ids`, `revoked_digests` and `packages`, each package an
    /// object with `archive` (path relative to the spec), `platforms` and
    /// `core`. Package id, runtime id, version and digest come from the
    /// archive.
    Build {
        /// Package spec file.
        #[arg(long, value_name = "FILE")]
        spec: PathBuf,
        /// Unsigned catalog file to write.
        #[arg(short, long, value_name = "FILE")]
        output: PathBuf,
    },
    /// Sign an unsigned catalog and verify the result before writing it.
    Sign {
        /// Unsigned catalog file from `catalog build`.
        #[arg(long, value_name = "FILE")]
        catalog: PathBuf,
        /// Signing key file (owner-private, 64 hex characters).
        #[arg(long, value_name = "FILE")]
        key_file: PathBuf,
        /// Key id the key file must have (64 hex characters).
        #[arg(long, value_name = "HEX")]
        key_id: String,
        /// Signed catalog file to write.
        #[arg(short, long, value_name = "FILE")]
        output: PathBuf,
    },
    /// Verify a signed catalog against a trust anchor file.
    Verify {
        /// Signed catalog file.
        #[arg(long, value_name = "FILE")]
        catalog: PathBuf,
        /// Trust anchor file.
        #[arg(long, value_name = "FILE")]
        anchor: PathBuf,
        /// Lowest acceptable catalog sequence.
        #[arg(long, value_name = "N")]
        high_water: Option<u64>,
    },
    /// Write the trust anchor file for one or more root public keys.
    ///
    /// Each `--root` states one root as
    /// `<public-key-file>:<not-before>:<not-after>`: the file holding the
    /// 64-hex public key (see `catalog public-key`), the first Unix second the
    /// root may sign and the Unix second from which it may no longer sign.
    /// There is no default window. Up to 8 distinct roots; the output lists
    /// them in ascending key id order regardless of argument order.
    Anchor {
        /// One trusted root; repeatable.
        #[arg(
            long = "root",
            value_name = "PUBLIC_KEY_FILE:NOT_BEFORE:NOT_AFTER",
            required = true
        )]
        roots: Vec<catalog::RootSpec>,
        /// Key id the anchor revokes; repeatable.
        #[arg(long = "revoked-key-id", value_name = "HEX")]
        revoked_key_ids: Vec<String>,
        /// Anchor file to write.
        #[arg(short, long, value_name = "FILE")]
        output: PathBuf,
    },
    /// Print the key id and public key of a signing key file.
    PublicKey {
        /// Signing key file (owner-private, 64 hex characters).
        #[arg(long, value_name = "FILE")]
        key_file: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum PackageAction {
    /// Build the canonical `tar.zst` archive of a package directory.
    ///
    /// With `--evidence-dir`, also write `<archive>.sha256`,
    /// `<runtime>.manifest.json`, `<runtime>.provenance-input.json` and
    /// `<runtime>.compat-vector.json` there. The directory must then be
    /// `runtime-packages/<runtime>` of a Git work tree, and its archived bytes,
    /// its compatibility lock and `compat/matrix.json` must equal the blobs of
    /// the commit `HEAD` names. A refusal or a failure while staging the five
    /// files writes nothing; they are renamed into place one by one only after
    /// all were written, so a failed rename can leave partial output.
    Build {
        /// Package directory holding `runtime.toml` and the package files.
        dir: PathBuf,
        /// Archive file to write.
        #[arg(short, long, value_name = "FILE")]
        output: PathBuf,
        /// Directory for the builder evidence; must lie outside the package
        /// directory and is created when missing (its parent must exist).
        #[arg(long, value_name = "DIR")]
        evidence_dir: Option<PathBuf>,
    },
    /// Build a package directory twice and require byte-identical archives.
    Verify {
        /// Package directory holding `runtime.toml` and the package files.
        dir: PathBuf,
    },
    /// Print the official runtime packages the current change must test.
    ///
    /// A file below `runtime-packages/<p>/` or `compat/<p>/` selects package
    /// `<p>`; a reviewed allowlist of prose paths selects nothing; every other
    /// file, and any file a Rust source embeds, is a core change and selects
    /// every package under `runtime-packages/`. Fails without printing a
    /// selection when Git or the file system fails.
    Affected(package_affected::Options),
    /// Print which official packages the signed release `v<version>` proves unchanged.
    ///
    /// A package is `verified` only when the catalog verifies against the
    /// anchor now and names that release, its entry has the digest of the
    /// package rebuilt from the work tree, the released archive and every
    /// referenced attestation hash to their catalog digests and bind the
    /// current lock and matrix, and the package and compatibility
    /// directories equal the release commit. Anything else is `missing` with
    /// a reason code and exits 0; only an unreadable checkout fails.
    Baseline(package_baseline::Options),
    /// Fail a package whose archive changed without a version above its released versions.
    ///
    /// Evaluates the same verdicts as `baseline` for release `v<version>`. A
    /// package whose archive differs from every released one fails when its
    /// `runtime.toml` version is not strictly above the released versions
    /// (semver). A catalog that is absent yields `undecided` and exit 0; a
    /// catalog that is present but does not verify, or names another release,
    /// fails. `--json` prints the decisions on stdout; the report goes to stderr.
    Bump(package_baseline::Options),
}

#[derive(Debug, Subcommand)]
enum DocsAction {
    Validate,
    Build,
    Check,
    Site,
    /// Manage the unreleased knowledge-log fragments under `docs/knowledge/log.d`.
    Log {
        #[command(subcommand)]
        action: LogAction,
    },
}

#[derive(Debug, Subcommand)]
enum LogAction {
    /// Write every fragment into `log.md` under `## <version>` and remove the
    /// fragments, as the release commit does.
    Fold {
        /// Release version the entries are filed under, such as `0.35.0`.
        #[arg(long, value_name = "X.Y.Z")]
        version: String,
        /// Print the fragments that would be folded and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Move every `## Unreleased (...)` section of `log.md` into a fragment.
    Migrate {
        /// Print the fragments that would be written and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
enum TsAction {
    Generate,
    Check,
}

#[derive(Debug, Subcommand)]
enum HermesAction {
    /// Run the model-free pinned Hermes compatibility checks.
    Compatibility {
        /// Hermes executable to inspect; defaults to PATH resolution.
        #[arg(long, value_name = "PATH", default_value = "hermes")]
        hermes_bin: PathBuf,
        /// Absolute directory holding the release `pohunek`, `pohunekd` and
        /// `pohunek-sessiond`. Each must be a canonical regular executable
        /// with no symlink component and no group or world write permission.
        #[arg(long, value_name = "DIR", required = true)]
        bin_dir: PathBuf,
        /// Absolute path of the Hermes runtime package archive (`tar.zst`).
        /// The gate signs a throwaway catalog for it, installs it through the
        /// copied daemon, and requires every installed lifecycle envelope to
        /// report it as the operator asset source.
        #[arg(long, value_name = "FILE", required = true)]
        package: PathBuf,
    },
    /// Write a signed catalog authorizing one Hermes package archive and the
    /// trust anchor of its throwaway signing key.
    ///
    /// The key comes from operating-system randomness and is never written;
    /// the catalog lists the package for this machine's platforms with
    /// placeholder attestations and expires after a day. A daemon trusts it
    /// only when the anchor file is placed beside its executable, so release
    /// smokes use it with a private copy of the binaries.
    ThrowawayCatalog {
        /// Hermes runtime package archive to authorize.
        #[arg(long, value_name = "FILE", required = true)]
        package: PathBuf,
        /// Directory to create; it receives `runtime-catalog-anchor.json` and
        /// `runtime-catalog.json`.
        #[arg(long, value_name = "DIR", required = true)]
        output_dir: PathBuf,
    },
    /// Refresh sanitized PTY goldens with an explicit Hermes executable.
    RefreshGoldens {
        /// Exact Hermes executable used for every capture.
        #[arg(long, value_name = "PATH", required = true)]
        hermes_bin: PathBuf,
    },
    /// Regenerate the checked Hermes skill from its knowledge source.
    GenerateSkill,
    /// Check that the checked Hermes skill is present and current.
    CheckSkill,
}

#[derive(Debug, Subcommand)]
enum AgentSkillAction {
    /// Regenerate the checked agent skill from its knowledge source.
    Generate,
}

fn run_package(action: PackageAction) -> Result<(), XtaskError> {
    match action {
        PackageAction::Build {
            dir,
            output,
            evidence_dir: None,
        } => {
            let digest = runtime_package::build(&dir, &output)?;
            println!("package build ok: {digest}");
        }
        PackageAction::Build {
            dir,
            output,
            evidence_dir: Some(evidence_dir),
        } => {
            let digest = package_evidence::build(&dir, &output, &evidence_dir)?;
            println!(
                "package build ok: {digest}, evidence in {}",
                evidence_dir.display()
            );
        }
        PackageAction::Verify { dir } => {
            let digest = runtime_package::verify(&dir)?;
            println!("package verify ok: reproducible build, {digest}");
        }
        PackageAction::Affected(options) => package_affected::run(&options)?,
        PackageAction::Baseline(options) => package_baseline::run(&options)?,
        PackageAction::Bump(options) => package_bump::run(&options)?,
    }
    Ok(())
}

fn run_catalog(action: CatalogAction) -> Result<(), XtaskError> {
    match action {
        CatalogAction::Build { spec, output } => {
            let entries = catalog::build_to(&spec, &output)?;
            println!("catalog build ok: {entries} entries");
        }
        CatalogAction::Sign {
            catalog,
            key_file,
            key_id,
            output,
        } => {
            let signer =
                catalog::sign_to(&catalog, &key_file, &key_id, &output, catalog::unix_now()?)?;
            println!("catalog sign ok: signed by {signer}");
        }
        CatalogAction::Verify {
            catalog,
            anchor,
            high_water,
        } => {
            let verified = catalog::verify(&catalog, &anchor, high_water, catalog::unix_now()?)?;
            println!(
                "catalog verify ok: sequence {}, expires_at {}, signer {}, {} entries",
                verified.sequence,
                verified.expires_at,
                verified.signer,
                verified.entries.len()
            );
            for entry in &verified.entries {
                println!("  {entry}");
            }
        }
        CatalogAction::Anchor {
            roots,
            revoked_key_ids,
            output,
        } => {
            catalog::anchor_to(&roots, &revoked_key_ids, &output)?;
            println!("catalog anchor ok: {} roots", roots.len());
        }
        CatalogAction::PublicKey { key_file } => {
            let (key_id, public_key) = catalog::public_key_of(&key_file)?;
            println!("key_id: {key_id}");
            println!("public_key: {public_key}");
        }
    }
    Ok(())
}

/// Root of the checkout the tool runs in.
///
/// Under test the root comes from the runtime environment, because the test
/// archive of CI runs from a relocated checkout while the compile-time path
/// names the checkout that built it.
#[cfg(test)]
fn repo_root() -> PathBuf {
    pohunek_test_support::workspace_root()
}

/// Root of the checkout the tool runs in.
#[cfg(not(test))]
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
