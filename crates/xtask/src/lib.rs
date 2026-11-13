#![forbid(unsafe_code)]

mod affected;
mod agent_skill;
mod attestation;
mod catalog;
mod catalog_key;
mod checks;
mod claude_canary;
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
mod plugin;
mod plugin_index;
pub mod ratchet;
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
    /// A plugin index document failed the daemon's verification rules.
    Index(::package::IndexError),
    /// An index entry failed an admission predicate; the text names it.
    Admission(String),
    /// A plugin proof ran and failed at a step; its evidence was written.
    Prove(plugin::Verdict),
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
    /// The command completed with a typed process exit code (0 is success and
    /// never reaches this variant).
    ExitCode(i32),
}

/// Process exit code for xtask failures that are not a delegated command's own exit.
const FAILURE_EXIT_CODE: i32 = 2;

/// Process exit code of a plugin index document that fails verification, so a
/// workflow tells a refused document from a refused invocation.
const INDEX_REFUSED_EXIT_CODE: i32 = 1;

/// Process exit code of a plugin proof that ran and failed, so a workflow
/// tells a failed proof from a refused invocation.
const PROVE_FAILED_EXIT_CODE: i32 = 1;

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
            Self::ExitCode(code) => *code,
            Self::Index(_) | Self::Admission(_) => INDEX_REFUSED_EXIT_CODE,
            Self::Prove(_) => PROVE_FAILED_EXIT_CODE,
            _ => FAILURE_EXIT_CODE,
        }
    }
}

impl fmt::Display for XtaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) | Self::Admission(message) => write!(f, "{message}"),
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
            Self::Index(error) => write!(f, "{error}"),
            Self::Prove(verdict) => write!(f, "{verdict}"),
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
            Self::ExitCode(code) => write!(f, "command exited with {code}"),
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
            Self::Index(error) => Some(error),
            Self::Prove(verdict) => Some(verdict),
            Self::Usage(_)
            | Self::Admission(_)
            | Self::KeyFile { .. }
            | Self::UnsupportedFileType(_)
            | Self::InvalidPath(_)
            | Self::OutputInsideInput(_)
            | Self::OutputIsSymlink(_)
            | Self::ChildExit { .. }
            | Self::ExitCode(_) => None,
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
    ensure_checkout()?;
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

/// Dispatch of the `claude` command; a nonzero outcome becomes the
/// [`XtaskError::ExitCode`] the process exits with (without an error line).
fn run_claude(action: claude_canary::ClaudeAction) -> Result<(), XtaskError> {
    let outcome = claude_canary::run(action)?;
    if outcome.exit_code() == 0 {
        Ok(())
    } else {
        Err(XtaskError::ExitCode(outcome.exit_code()))
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "one flat dispatch over the top-level commands is clearer than splitting it"
)]
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

    ensure_checkout()?;
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
        TopCommand::Claude { action } => run_claude(action),
        TopCommand::Affected(options) => affected::run(&root, &options),
        TopCommand::Package { action } => run_package(action),
        TopCommand::Catalog { action } => run_catalog(action),
        TopCommand::Index { action } => run_index(action),
        TopCommand::Plugin { action } => run_plugin(action),
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
    /// The Claude OSC 7501 canary.
    ///
    /// `compare` diffs a capture against the expected table. A row whose
    /// expected value is `null` is UNVERIFIED by definition and never a
    /// pass; expectations change only by a reviewed hand edit.
    Claude {
        #[command(subcommand)]
        action: claude_canary::ClaudeAction,
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
    /// Owner ceremony and index tooling commands of the plugin index.
    ///
    /// Keys are read only from files that pass the same rules as
    /// `catalog sign` and are never printed; the index tooling (`build`,
    /// `message`, `attach`, `verify`, `throwaway`) reads no key file at all.
    /// Every output file must not exist yet (`throwaway` takes an existing empty
    /// directory); a refused input writes nothing.
    Index {
        #[command(subcommand)]
        action: IndexAction,
    },
    /// Build and prove the archive of a plugin release.
    ///
    /// `build` writes the canonical archive of a package directory; `prove`
    /// installs an archive through the plugin index path on an isolated daemon
    /// and writes the verdict as evidence. Neither reads a key file, and
    /// `prove` never launches the runtime program of the package.
    Plugin {
        #[command(subcommand)]
        action: PluginAction,
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
    /// Create a new Ed25519 signing key file.
    ///
    /// The 32-byte seed comes from operating-system randomness and is written
    /// as 64 lowercase hex characters and a newline to a new file with mode
    /// 0600; an existing path is refused. Prints `key_id=<hex>` and
    /// `public_key=<hex>`, never the seed.
    Keygen {
        /// New key file to create; the path must not exist.
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

/// Predicate group of `index check-entry`.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum CheckStageArg {
    Schema,
    CoreRange,
}

#[derive(Debug, Subcommand)]
enum IndexAction {
    /// Endorse an index key: write its signed `official` key record.
    ///
    /// The window must span at least one index lifetime (14 days), at most
    /// 366 days, and end in the future. The record is verified against the
    /// endorser's public key before it is written.
    Endorse {
        /// Key file of the endorsing root (owner-private, 64 hex characters).
        #[arg(long, value_name = "FILE")]
        endorser_key_file: PathBuf,
        /// Key file of the index key to endorse (owner-private).
        #[arg(long, value_name = "FILE")]
        key_file: PathBuf,
        /// Scope of the record; only `official` exists.
        #[arg(long, value_name = "SCOPE")]
        scope: String,
        /// First Unix second the index key may sign.
        #[arg(long, value_name = "UNIX_SECONDS")]
        not_before: u64,
        /// Unix second from which the index key may no longer sign.
        #[arg(long, value_name = "UNIX_SECONDS")]
        not_after: u64,
        /// Record file to write.
        #[arg(short, long, value_name = "FILE")]
        output: PathBuf,
    },
    /// Write a signed revocation list for index keys.
    ///
    /// The new list is exactly the `--key-id` values. With `--previous` the
    /// sequence must exceed the previous one and the list must keep every
    /// previously revoked id; without it only `--sequence 1` is accepted. At
    /// most 64 ids.
    Revoke {
        /// Key file of the endorsing root (owner-private, 64 hex characters).
        #[arg(long, value_name = "FILE")]
        endorser_key_file: PathBuf,
        /// Sequence of the new document.
        #[arg(long, value_name = "N")]
        sequence: u64,
        /// Key id to revoke; repeatable.
        #[arg(long = "key-id", value_name = "HEX")]
        key_ids: Vec<String>,
        /// Previous revocation document, signed by the same endorser.
        #[arg(long, value_name = "FILE")]
        previous: Option<PathBuf>,
        /// Revocation document to write.
        #[arg(short, long, value_name = "FILE")]
        output: PathBuf,
    },
    /// Build the unsigned index document from an entries tree.
    ///
    /// The tree is `<entries>/official/<package_id>/<version>.toml` and
    /// nothing else. The `[source]` table of each entry is validated and left
    /// out of the document. The result must pass the daemon's body rules
    /// before it is written. With `--print-source`, print the validated
    /// `[source]` of the tree's only entry as `key=value` lines instead.
    Build {
        /// Entries tree.
        #[arg(long, value_name = "DIR")]
        entries: PathBuf,
        /// Print the validated `[source]` of the only entry and write nothing.
        #[arg(
            long,
            conflicts_with_all = ["revocations", "key_record", "sequence", "expires_at", "output"]
        )]
        print_source: bool,
        /// Schema of the document: 1 lists runtime entries only and is read by
        /// every release since 0.40; 2 also lists `context` entries and is read
        /// from 0.42.
        #[arg(long, value_name = "VERSION", default_value_t = 1)]
        schema_version: u64,
        /// Signed revocation list to embed.
        #[arg(long, value_name = "FILE", required_unless_present = "print_source")]
        revocations: Option<PathBuf>,
        /// Index key record to embed; repeatable, 1 to 4.
        #[arg(
            long = "key-record",
            value_name = "FILE",
            required_unless_present = "print_source"
        )]
        key_record: Vec<PathBuf>,
        /// Anti-rollback sequence: the Unix second of signing.
        #[arg(
            long,
            value_name = "UNIX_SECONDS",
            required_unless_present = "print_source"
        )]
        sequence: Option<u64>,
        /// Unix second from which the document must no longer be accepted.
        #[arg(
            long,
            value_name = "UNIX_SECONDS",
            required_unless_present = "print_source"
        )]
        expires_at: Option<u64>,
        /// Unsigned index file to write.
        #[arg(
            short,
            long,
            value_name = "FILE",
            required_unless_present = "print_source"
        )]
        output: Option<PathBuf>,
    },
    /// Check the admission predicates of one added index entry.
    ///
    /// `--entries` holds exactly the added entry and `--existing` the admitted
    /// `official/` tree. Stage `schema` is step 1 of the admission and prints
    /// the validated entry fields as `key=value` lines; stage `core-range` is
    /// step 7. A refused entry exits 1 and names `predicate=<name>`.
    CheckEntry {
        /// Entries tree holding only the added entry.
        #[arg(long, value_name = "DIR")]
        entries: PathBuf,
        /// Entries tree of the admitted entries.
        #[arg(long, value_name = "DIR")]
        existing: PathBuf,
        /// Version of the pinned core, without the leading `v`.
        #[arg(long, value_name = "X.Y.Z")]
        core_version: String,
        /// Predicate group to run.
        #[arg(long, value_enum)]
        stage: CheckStageArg,
    },
    /// Write the message an index signature covers.
    ///
    /// The file is the index domain prefix followed by the canonical bytes of
    /// the document's `index` member as parsed. The signing key never reaches
    /// this tool: sign the file with a tool outside the workspace.
    Message {
        /// Unsigned index file.
        #[arg(long, value_name = "FILE")]
        unsigned: PathBuf,
        /// Message file to write.
        #[arg(short, long, value_name = "FILE")]
        output: PathBuf,
    },
    /// Attach an externally produced signature to an unsigned index.
    ///
    /// The signature file holds 128 lowercase hex characters and an optional
    /// newline. The signed document is verified with the daemon's rules under
    /// the endorser the channel file names, and written only when the signer
    /// is `--key-id`.
    Attach {
        /// Unsigned index file.
        #[arg(long, value_name = "FILE")]
        unsigned: PathBuf,
        /// Key id of the index key record that signed.
        #[arg(long, value_name = "HEX")]
        key_id: String,
        /// File with the signature in hex.
        #[arg(long, value_name = "FILE")]
        signature: PathBuf,
        /// Trust anchor file.
        #[arg(long, value_name = "FILE")]
        anchor: PathBuf,
        /// Plugin index channel file.
        #[arg(long, value_name = "FILE")]
        channel: PathBuf,
        /// Signed index file to write.
        #[arg(short, long, value_name = "FILE")]
        output: PathBuf,
    },
    /// Verify a signed index, or a key record and a revocation list.
    ///
    /// `--index` runs the daemon's verifier at the current time with the
    /// anchor's revoked key ids. `--record` and `--revocations` check key
    /// material for an index repository change; `--previous-revocations`
    /// additionally requires a higher sequence and a superset of revoked ids.
    Verify {
        /// Signed index file.
        #[arg(
            long,
            value_name = "FILE",
            conflicts_with_all = ["record", "revocations", "previous_revocations"],
            required_unless_present_any = ["record", "revocations"]
        )]
        index: Option<PathBuf>,
        /// Index key record file.
        #[arg(long, value_name = "FILE", requires = "revocations")]
        record: Option<PathBuf>,
        /// Signed revocation list file.
        #[arg(long, value_name = "FILE", requires = "record")]
        revocations: Option<PathBuf>,
        /// Previous signed revocation list file.
        #[arg(long, value_name = "FILE", requires = "revocations")]
        previous_revocations: Option<PathBuf>,
        /// Trust anchor file.
        #[arg(long, value_name = "FILE")]
        anchor: PathBuf,
        /// Plugin index channel file.
        #[arg(long, value_name = "FILE")]
        channel: PathBuf,
        /// Highest index sequence accepted before.
        #[arg(long, value_name = "N", requires = "index")]
        index_high_water: Option<u64>,
        /// Highest revocation sequence accepted before.
        #[arg(long, value_name = "N", requires = "index")]
        revocation_high_water: Option<u64>,
    },
    /// Create a throwaway trust root and a signed index for proofs.
    ///
    /// A random root and index key sign the index of `--entries` and are
    /// discarded; neither seed is written. `--output` must be an existing
    /// empty directory outside the checkout. It receives
    /// `runtime-catalog-anchor.json` (the throwaway root only),
    /// `plugin-index-channel.json` and `official-index-v1.json`; no release
    /// anchor trusts them.
    Throwaway {
        /// Entries tree.
        #[arg(long, value_name = "DIR")]
        entries: PathBuf,
        /// Existing empty directory that receives the three files.
        #[arg(long, value_name = "DIR")]
        output: PathBuf,
    },
    /// Write the channel document that names the index endorser root.
    ///
    /// The id must be a root of the trust anchor.
    Channel {
        /// Trust anchor file.
        #[arg(long, value_name = "FILE")]
        anchor: PathBuf,
        /// Key id of the endorsing root (64 hex characters).
        #[arg(long, value_name = "HEX")]
        endorser_key_id: String,
        /// Channel file to write.
        #[arg(short, long, value_name = "FILE")]
        output: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum PluginAction {
    /// Build the canonical archive of a package directory.
    ///
    /// The archive is written as `<package_id>-<version>.tar.zst` into
    /// `--output-dir`, which must exist and lie outside the package directory.
    /// An existing file is an error and is left unchanged. Prints
    /// `plugin build ok: <package_id> <version> <digest> <path>`.
    Build {
        /// Package directory holding `runtime.toml` and the package files.
        #[arg(long, value_name = "DIR")]
        dir: PathBuf,
        /// Existing directory that receives the archive.
        #[arg(long, value_name = "DIR")]
        output_dir: PathBuf,
    },
    /// Install an archive through the index path on an isolated daemon.
    ///
    /// Steps: `digest` (the archive is the one the entry lists), `trust` (a
    /// throwaway index that lists only that entry), `host` (the release
    /// executables from `--release-dir` start in a private prefix),
    /// `install`, `inspect`, and `cleanup` (the daemon stops and the prefix
    /// is removed). The evidence file is written for a pass and for a failed
    /// step; the exit status is 0 only for a pass, 1 for a failed step and 2
    /// for a refused invocation.
    Prove {
        /// Package archive to prove.
        #[arg(long, value_name = "FILE")]
        archive: PathBuf,
        /// Index entry file of the archive, as in the index repository.
        #[arg(long, value_name = "FILE")]
        entry: PathBuf,
        /// Directory that holds `pohunek`, `pohunekd` and `pohunek-sessiond`.
        #[arg(long, value_name = "DIR")]
        release_dir: PathBuf,
        /// Evidence file to write; it must not exist yet.
        #[arg(long, value_name = "FILE")]
        evidence: PathBuf,
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
        CatalogAction::Keygen { output } => {
            let key = plugin_index::keygen(&output)?;
            println!("key_id={}", key.key_id);
            println!("public_key={}", key.public_key);
        }
        CatalogAction::PublicKey { key_file } => {
            let (key_id, public_key) = catalog::public_key_of(&key_file)?;
            println!("key_id: {key_id}");
            println!("public_key: {public_key}");
        }
    }
    Ok(())
}

/// `index build`, with or without `--print-source`.
fn run_index_build(action: IndexAction) -> Result<(), XtaskError> {
    let IndexAction::Build {
        entries,
        print_source,
        schema_version,
        revocations,
        key_record,
        sequence,
        expires_at,
        output,
    } = action
    else {
        return Ok(());
    };

    if print_source {
        print!("{}", plugin_index::print_source(&entries)?);
    } else {
        // clap requires each of these unless `--print-source` is given.
        let (Some(revocations), Some(sequence), Some(expires_at), Some(output)) =
            (revocations, sequence, expires_at, output)
        else {
            return Err(XtaskError::Usage(
                "index build needs --revocations, --sequence, --expires-at and --output".into(),
            ));
        };
        let count = plugin_index::build(
            &plugin_index::BuildArgs {
                entries,
                revocations,
                key_records: key_record,
                sequence,
                expires_at,
                output,
                schema_version,
            },
            catalog::unix_now()?,
        )?;
        println!("index build ok: entries={count}");
    }
    Ok(())
}

/// `index check-entry`: prints the validated fields or the success line.
fn run_index_check_entry(action: IndexAction) -> Result<(), XtaskError> {
    let IndexAction::CheckEntry {
        entries,
        existing,
        core_version,
        stage,
    } = action
    else {
        return Ok(());
    };
    let stage = match stage {
        CheckStageArg::Schema => plugin_index::CheckStage::Schema,
        CheckStageArg::CoreRange => plugin_index::CheckStage::CoreRange,
    };
    print!(
        "{}",
        plugin_index::check_entry(&plugin_index::CheckArgs {
            entries,
            existing,
            core_version,
            stage,
        })?
    );
    Ok(())
}

/// The index commands that read no key file.
fn run_index_tooling(action: IndexAction) -> Result<(), XtaskError> {
    match action {
        build @ IndexAction::Build { .. } => run_index_build(build)?,
        IndexAction::Message { unsigned, output } => {
            let written = plugin_index::message(&unsigned, &output)?;
            println!(
                "index message ok: sha256={} bytes={}",
                written.sha256, written.bytes
            );
        }
        IndexAction::Attach {
            unsigned,
            key_id,
            signature,
            anchor,
            channel,
            output,
        } => {
            plugin_index::attach(
                &plugin_index::AttachArgs {
                    unsigned,
                    key_id,
                    signature,
                    anchor,
                    channel,
                    output,
                },
                catalog::unix_now()?,
            )?;
            println!("index attach ok");
        }
        IndexAction::Verify {
            index,
            record,
            revocations,
            previous_revocations,
            anchor,
            channel,
            index_high_water,
            revocation_high_water,
        } => match (index, record, revocations) {
            (Some(index), None, None) => {
                let verified = plugin_index::verify_whole(
                    &plugin_index::VerifyIndexArgs {
                        index,
                        anchor,
                        channel,
                        index_high_water,
                        revocation_high_water,
                    },
                    catalog::unix_now()?,
                )?;
                println!(
                    "index verify ok: sequence={} expires_at={} signer={} entries={}",
                    verified.sequence, verified.expires_at, verified.signer, verified.entries
                );
            }
            (None, Some(record), Some(revocations)) => {
                let verified = plugin_index::verify_material(&plugin_index::VerifyMaterialArgs {
                    record,
                    revocations,
                    previous_revocations,
                    anchor,
                    channel,
                })?;
                println!(
                    "material verify ok: key_id={} revocation_sequence={}",
                    verified.key_id, verified.revocation_sequence
                );
            }
            _ => {
                return Err(XtaskError::Usage(
                    "index verify takes --index, or --record with --revocations".into(),
                ));
            }
        },
        check @ IndexAction::CheckEntry { .. } => run_index_check_entry(check)?,
        IndexAction::Throwaway { entries, output } => {
            let written =
                plugin_index::throwaway(&plugin_index::ThrowawayArgs { entries, output })?;
            println!(
                "index throwaway ok: entries={} endorser={} signer={}",
                written.entries, written.endorser, written.signer
            );
        }
        IndexAction::Endorse { .. } | IndexAction::Revoke { .. } | IndexAction::Channel { .. } => {
            unreachable!("run_index sends only the tooling commands here")
        }
    }
    Ok(())
}

fn run_plugin(action: PluginAction) -> Result<(), XtaskError> {
    match action {
        PluginAction::Build { dir, output_dir } => {
            let built = plugin::build(&plugin::BuildArgs { dir, output_dir })?;
            println!("{}", built.line());
        }
        PluginAction::Prove {
            archive,
            entry,
            release_dir,
            evidence,
        } => {
            let report = plugin::prove(&plugin::ProveArgs {
                archive,
                entry,
                release_dir,
                evidence,
            })?;
            println!("{report}");
        }
    }
    Ok(())
}

fn run_index(action: IndexAction) -> Result<(), XtaskError> {
    match action {
        IndexAction::Endorse {
            endorser_key_file,
            key_file,
            scope,
            not_before,
            not_after,
            output,
        } => {
            let endorsed = plugin_index::endorse(
                &plugin_index::EndorseArgs {
                    endorser_key_file,
                    key_file,
                    scope,
                    not_before,
                    not_after,
                    output,
                },
                catalog::unix_now()?,
            )?;
            println!("key_id={}", endorsed.key_id);
            println!("endorsed_by={}", endorsed.endorsed_by);
        }
        IndexAction::Revoke {
            endorser_key_file,
            sequence,
            key_ids,
            previous,
            output,
        } => {
            let revoked = plugin_index::revoke(&plugin_index::RevokeArgs {
                endorser_key_file,
                sequence,
                key_ids,
                previous,
                output,
            })?;
            println!(
                "index revoke ok: sequence={} revoked={}",
                revoked.sequence, revoked.revoked
            );
        }
        tooling @ (IndexAction::Build { .. }
        | IndexAction::CheckEntry { .. }
        | IndexAction::Message { .. }
        | IndexAction::Attach { .. }
        | IndexAction::Verify { .. }
        | IndexAction::Throwaway { .. }) => run_index_tooling(tooling)?,
        IndexAction::Channel {
            anchor,
            endorser_key_id,
            output,
        } => {
            let args = plugin_index::ChannelArgs {
                anchor,
                endorser_key_id,
                output,
            };
            plugin_index::channel(&args)?;
            println!("index channel ok: endorser={}", args.endorser_key_id);
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

/// Under test the checkout is the workspace of the test run.
#[cfg(test)]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the signature matches the run-time variant, which can refuse the working directory"
)]
fn ensure_checkout() -> Result<(), XtaskError> {
    Ok(())
}

/// Manifest whose presence marks the root of a checkout this tool belongs to.
#[cfg(not(test))]
const XTASK_MANIFEST: &str = "crates/xtask/Cargo.toml";

/// The nearest ancestor of the working directory that holds the xtask manifest.
#[cfg(not(test))]
fn find_checkout() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    cwd.ancestors()
        .find(|directory| directory.join(XTASK_MANIFEST).is_file())
        .map(Path::to_path_buf)
}

/// Refuses a working directory outside every checkout.
///
/// The root is resolved at run time. A binary built in one checkout and reused
/// from another through a shared `CARGO_TARGET_DIR` would otherwise act on the
/// checkout that compiled it.
#[cfg(not(test))]
fn ensure_checkout() -> Result<(), XtaskError> {
    if find_checkout().is_some() {
        return Ok(());
    }
    let cwd = std::env::current_dir().map_err(|source| XtaskError::Io {
        path: PathBuf::from("."),
        source,
    })?;
    Err(XtaskError::Usage(format!(
        "the working directory {} is not inside a pohunek checkout (no ancestor holds {XTASK_MANIFEST})",
        cwd.display()
    )))
}

/// Root of the checkout the tool runs in; `ensure_checkout` has accepted the
/// working directory by the time a command asks.
#[cfg(not(test))]
fn repo_root() -> PathBuf {
    find_checkout().expect("ensure_checkout accepted the working directory")
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
