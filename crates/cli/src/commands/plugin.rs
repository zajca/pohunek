//! `pohunek plugin` runtime package lifecycle commands.
//!
//! Every subcommand is a request to the daemon on this machine's local control
//! socket: installing a package extends the owner's launch authority on that
//! machine, so the commands reject any other effective host before any I/O.
//! Installation and removal follow one consent pattern: the daemon validates
//! the package in a dry run, the CLI prints what it declares, and nothing
//! changes until the owner repeats the command with `--yes`.

// Rust guideline compliant 2026-10-04

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;

use clap::{Args, Subcommand, ValueHint};
use protocol::{
    method, ErrorClass, PackageChangeResult, PackageDigest, PackageDoctorParams,
    PackageDoctorResult, PackageFault, PackageFindingKind, PackageId, PackageInfo,
    PackageInspectParams, PackageInspectResult, PackageInstallParams, PackageInstallResult,
    PackageInstallStatus, PackageLinkParams, PackageListResult, PackageOrigin, PackageRuntimeInfo,
    PackageSelectParams, PackageSetEnabledParams, PackageTrust, PackageUninstallParams,
    PackageUninstallResult, PackageVersion,
};

use crate::client::Client;
use crate::commands::plugin_profile::{self, ProfileAction};
use crate::commands::render_json;
use crate::error::CliError;
use crate::paths::Paths;
use crate::target::{is_local_host, LOCAL_HOST};

/// Prefix every package digest carries on the wire.
const DIGEST_PREFIX: &str = "sha256:";

/// Number of hex characters in a full SHA-256 digest.
const DIGEST_HEX_CHARS: usize = 64;

/// Hex characters of a digest shown in tables, and the shortest prefix
/// accepted by `--digest`.
///
/// Both share one value so a digest copied from a table is always a valid
/// selector. Twelve hex characters are 48 bits: collisions among the handful
/// of packages one host holds are not a practical concern, and an ambiguous
/// prefix is reported rather than guessed.
const DIGEST_PREFIX_HEX_CHARS: usize = 12;

/// `pohunek plugin` subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum Action {
    /// List installed runtime packages and their health.
    List {
        /// Emit machine-readable JSON instead of a table.
        #[arg(long)]
        json: bool,
    },

    /// Show one installed package: its record, health, and the runtime it declares.
    ///
    /// With several installed versions and no `--digest` or `--version`, the
    /// selected version is shown.
    Inspect {
        #[command(flatten)]
        selector: SelectorArgs,
        /// Emit machine-readable JSON instead of text.
        #[arg(long)]
        json: bool,
    },

    /// Install a package archive.
    ///
    /// The daemon validates the archive and this command prints what the
    /// package declares, including the program it would launch as you. Nothing
    /// is installed until you repeat the command with `--yes`.
    ///
    /// Trust is explicit: a third-party archive is authorized only by the
    /// digest you supply with `--sha256`; `--catalog` authorizes official
    /// packages through a signed catalog file.
    Install(InstallArgs),

    /// Copy a package directory into storage for development.
    ///
    /// The package is installed disabled and unselected; enable it with
    /// `pohunek plugin enable`. Nothing is installed until you repeat the
    /// command with `--yes`.
    Link(LinkArgs),

    /// Install a new version of an installed package and select it.
    ///
    /// The previous version stays installed, so `pohunek plugin select` can
    /// roll back to it; profiles keep the digest they pinned. Nothing is
    /// installed until you repeat the command with `--yes`.
    Update(UpdateArgs),

    /// Select which installed version bare requests of the package resolve to.
    Select {
        #[command(flatten)]
        selector: SelectorArgs,
        /// Emit machine-readable JSON instead of text.
        #[arg(long)]
        json: bool,
    },

    /// Allow fresh sessions to launch the package.
    Enable {
        #[command(flatten)]
        selector: SelectorArgs,
        /// Emit machine-readable JSON instead of text.
        #[arg(long)]
        json: bool,
    },

    /// Block fresh sessions from launching the package.
    ///
    /// Live sessions keep running and an existing session pinned to the
    /// package can still be resumed.
    Disable {
        #[command(flatten)]
        selector: SelectorArgs,
        /// Emit machine-readable JSON instead of text.
        #[arg(long)]
        json: bool,
    },

    /// Remove an installed package that no session or profile pins.
    ///
    /// Refuses while a session or profile pins the digest. Nothing is removed
    /// until you repeat the command with `--yes`.
    Uninstall(UninstallArgs),

    /// Verify installed package roots and report problems.
    ///
    /// Exits with status 1 when any finding exists.
    Doctor {
        /// Restrict the report to the versions of one package id.
        #[arg(value_name = "PACKAGE")]
        package: Option<PackageId>,
        /// Emit machine-readable JSON instead of a table.
        #[arg(long)]
        json: bool,
    },

    /// Inspect and migrate host agent profiles that run on a package.
    ///
    /// A profile whose base runtime is served by an installed package pins
    /// that package by `package` and `digest`; it moves to another digest only
    /// through `pohunek plugin profile migrate`.
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
    },
}

impl Action {
    /// Whether the subcommand requested `--json` output.
    pub(crate) fn wants_json(&self) -> bool {
        match self {
            Self::List { json }
            | Self::Inspect { json, .. }
            | Self::Select { json, .. }
            | Self::Enable { json, .. }
            | Self::Disable { json, .. }
            | Self::Doctor { json, .. } => *json,
            Self::Profile { action } => action.wants_json(),
            Self::Install(args) => args.json,
            Self::Link(args) => args.json,
            Self::Update(args) => args.json,
            Self::Uninstall(args) => args.json,
        }
    }
}

/// Names the installed package version a command acts on.
#[derive(Debug, Clone, Args)]
pub(crate) struct SelectorArgs {
    /// Package id, as shown by `pohunek plugin list`.
    #[arg(value_name = "PACKAGE")]
    package: PackageId,
    /// Narrow to the version with this digest: the full `sha256:<64 hex>` or a
    /// unique hex prefix of at least 12 characters.
    #[arg(long, value_name = "DIGEST")]
    digest: Option<DigestSelector>,
    /// Narrow to this exact version.
    #[arg(long, value_name = "VERSION")]
    version: Option<PackageVersion>,
}

/// How an archive is authorized.
#[derive(Debug, Clone, Args)]
#[group(required = true, multiple = false)]
pub(crate) struct TrustArgs {
    /// Authorize the archive by its SHA-256 digest (`sha256:<64 hex>`, or the
    /// bare hex `sha256sum` prints). Third-party packages are trusted only by
    /// a digest the owner supplies; this never authorizes an official runtime.
    #[arg(long, value_name = "DIGEST", value_parser = parse_archive_digest)]
    sha256: Option<PackageDigest>,
    /// Authorize the archive as an official package through this signed
    /// catalog file.
    #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath)]
    catalog: Option<PathBuf>,
}

/// Arguments of `pohunek plugin install`.
#[derive(Debug, Clone, Args)]
pub(crate) struct InstallArgs {
    /// Package archive to install.
    #[arg(value_name = "ARCHIVE", value_hint = ValueHint::FilePath)]
    archive: PathBuf,
    #[command(flatten)]
    trust: TrustArgs,
    /// Install the package disabled.
    #[arg(long)]
    no_enable: bool,
    /// Consent to the install after reviewing what the package declares.
    #[arg(long)]
    yes: bool,
    /// Emit machine-readable JSON instead of text.
    #[arg(long)]
    json: bool,
}

/// Arguments of `pohunek plugin link`.
#[derive(Debug, Clone, Args)]
pub(crate) struct LinkArgs {
    /// Package directory to copy.
    #[arg(value_name = "DIR", value_hint = ValueHint::DirPath)]
    directory: PathBuf,
    /// Consent to the install after reviewing what the package declares.
    #[arg(long)]
    yes: bool,
    /// Emit machine-readable JSON instead of text.
    #[arg(long)]
    json: bool,
}

/// Arguments of `pohunek plugin update`.
#[derive(Debug, Clone, Args)]
pub(crate) struct UpdateArgs {
    /// Installed package to update.
    #[arg(value_name = "PACKAGE")]
    package: PackageId,
    /// Archive holding the new version.
    #[arg(value_name = "ARCHIVE", value_hint = ValueHint::FilePath)]
    archive: PathBuf,
    #[command(flatten)]
    trust: TrustArgs,
    /// Consent to the update after reviewing what the package declares.
    #[arg(long)]
    yes: bool,
    /// Emit machine-readable JSON instead of text.
    #[arg(long)]
    json: bool,
}

/// Arguments of `pohunek plugin uninstall`.
#[derive(Debug, Clone, Args)]
pub(crate) struct UninstallArgs {
    #[command(flatten)]
    selector: SelectorArgs,
    /// Remove a package whose root fails verification. A root that verifies is
    /// refused: uninstall it without this flag.
    #[arg(long)]
    remove_modified: bool,
    /// Consent to the removal.
    #[arg(long)]
    yes: bool,
    /// Emit machine-readable JSON instead of text.
    #[arg(long)]
    json: bool,
}

/// A `--digest` selector: a full digest or a prefix of its hex part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DigestSelector {
    /// The complete digest.
    Full(PackageDigest),
    /// Lowercase hex characters the digest must start with.
    Prefix(String),
}

impl DigestSelector {
    pub(super) fn matches(&self, digest: &PackageDigest) -> bool {
        match self {
            Self::Full(full) => full == digest,
            Self::Prefix(prefix) => digest
                .as_str()
                .strip_prefix(DIGEST_PREFIX)
                .is_some_and(|hex| hex.starts_with(prefix.as_str())),
        }
    }
}

impl FromStr for DigestSelector {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let hex = value
            .strip_prefix(DIGEST_PREFIX)
            .unwrap_or(value)
            .to_ascii_lowercase();
        if hex.is_empty() || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!(
                "expected `{DIGEST_PREFIX}<{DIGEST_HEX_CHARS} hex>` or a hex prefix"
            ));
        }
        if hex.len() == DIGEST_HEX_CHARS {
            return PackageDigest::parse(&format!("{DIGEST_PREFIX}{hex}"))
                .map(Self::Full)
                .map_err(|error| error.to_string());
        }
        if hex.len() < DIGEST_PREFIX_HEX_CHARS {
            return Err(format!(
                "a digest prefix needs at least {DIGEST_PREFIX_HEX_CHARS} hex characters"
            ));
        }
        if hex.len() > DIGEST_HEX_CHARS {
            return Err(format!("a digest has {DIGEST_HEX_CHARS} hex characters"));
        }
        Ok(Self::Prefix(hex))
    }
}

/// Parse an archive digest: `sha256:<64 hex>` or the bare 64 hex characters.
fn parse_archive_digest(value: &str) -> Result<PackageDigest, String> {
    let hex = value.strip_prefix(DIGEST_PREFIX).unwrap_or(value);
    PackageDigest::parse(&format!("{DIGEST_PREFIX}{hex}")).map_err(|_malformed| {
        format!("expected `{DIGEST_PREFIX}<{DIGEST_HEX_CHARS} lowercase hex>`")
    })
}

/// Failures specific to the plugin commands.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    /// A remote host was selected for a local-only command.
    #[error(
        "plugin commands manage the daemon on this machine only; --host {host:?} is not supported"
    )]
    LocalOnly {
        /// The rejected effective host.
        host: String,
    },

    /// A source path does not name an existing file or directory of the right kind.
    #[error("{path}: not an existing {expected}")]
    InvalidSource {
        /// The path as typed.
        path: String,
        /// `file` or `directory`.
        expected: &'static str,
    },

    /// The action needs the owner's consent and `--yes` was not given.
    #[error("{verb} {summary} needs your consent; nothing was changed")]
    ConsentRequired {
        /// Gerund describing the refused action.
        verb: &'static str,
        /// Package id, version, and abbreviated digest.
        summary: String,
    },

    /// No installed package has the requested id.
    #[error("no installed package has the id {package}")]
    NotInstalled {
        /// The requested package id.
        package: String,
    },

    /// The package is installed but no version matches the narrowing options.
    #[error("package {package} is installed but no version matches the given selector")]
    NoMatchingVersion {
        /// The requested package id.
        package: String,
    },

    /// Several installed versions match and the command does not guess.
    #[error("package {package} has several matching installed versions: {candidates}")]
    Ambiguous {
        /// The requested package id.
        package: String,
        /// Sorted `version (digest)` list.
        candidates: String,
    },

    /// The archive of `update` holds another package than the one named.
    #[error("the archive holds package {found}, not {expected}")]
    UpdateIdMismatch {
        /// The package id named on the command line.
        expected: String,
        /// The package id the archive declares.
        found: String,
    },

    /// The agents directory is missing its safety properties or unreadable.
    #[error("the agent profiles directory cannot be used: {detail}")]
    ProfileDirectory {
        /// What is wrong, without file content.
        detail: String,
    },

    /// No profile file has the requested name.
    #[error("no agent profile named {name} exists on this host")]
    ProfileNotFound {
        /// The requested profile name.
        name: String,
    },

    /// The profile file fails the safety policy, cannot be read, or does not parse.
    #[error("profile {name} cannot be used: {detail}")]
    ProfileUnusable {
        /// The profile name.
        name: String,
        /// The reason, naming a key or line but never a value.
        detail: String,
    },

    /// No installed package serves the profile's base runtime.
    #[error(
        "profile {name} extends base runtime {base}, which no installed package serves; there is nothing to migrate"
    )]
    ProfileBaseBuiltin {
        /// The profile name.
        name: String,
        /// The profile's base runtime id.
        base: String,
    },

    /// The migration target does not resolve to one usable package.
    #[error("profile {name}: {detail}")]
    ProfileTarget {
        /// The profile name.
        name: String,
        /// Why no single target resolves.
        detail: String,
    },

    /// Rewriting the profile failed.
    #[error("profile {name} was not rewritten: {detail}")]
    ProfileWrite {
        /// The profile name.
        name: String,
        /// What failed.
        detail: String,
    },
}

impl Error {
    /// Stable wire error code.
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::LocalOnly { .. } => "plugin_local_only",
            Self::InvalidSource { .. } => "plugin_source_invalid",
            Self::ConsentRequired { .. } => "consent_required",
            Self::NotInstalled { .. } | Self::NoMatchingVersion { .. } => "package_not_installed",
            Self::Ambiguous { .. } => "plugin_selector_ambiguous",
            Self::UpdateIdMismatch { .. } => "plugin_update_id_mismatch",
            Self::ProfileDirectory { .. } => "profile_directory_unusable",
            Self::ProfileNotFound { .. } => "profile_not_found",
            Self::ProfileUnusable { .. } => "profile_unusable",
            Self::ProfileBaseBuiltin { .. } => "profile_base_builtin",
            Self::ProfileTarget { .. } => "profile_target_invalid",
            Self::ProfileWrite { .. } => "profile_write_failed",
        }
    }

    /// Recovery hint shown beneath the error.
    pub(crate) fn hint(&self) -> &'static str {
        match self {
            Self::LocalOnly { .. } => {
                "run the command on the machine that runs the daemon, without --host"
            }
            Self::InvalidSource { .. } => "pass an existing path of the expected kind",
            Self::ConsentRequired { .. } => {
                "review the package details (omit --json to see them), then repeat the command with --yes"
            }
            Self::NotInstalled { .. } => "list installed packages with `pohunek plugin list`",
            Self::NoMatchingVersion { .. } => {
                "list installed versions with `pohunek plugin list` and adjust --digest or --version"
            }
            Self::Ambiguous { .. } => "narrow the selection with --digest or --version",
            Self::UpdateIdMismatch { .. } => {
                "name the package id the archive declares, or use `pohunek plugin install`"
            }
            Self::ProfileDirectory { .. } => {
                "the agents directory must be owned by you and not group- or world-writable"
            }
            Self::ProfileNotFound { .. } => "list profiles with `pohunek plugin profile list`",
            Self::ProfileUnusable { .. } => {
                "fix the profile file: it must be a regular file you own, not writable by others, with valid TOML"
            }
            Self::ProfileBaseBuiltin { .. } => {
                "install a package that serves the base runtime with `pohunek plugin install`"
            }
            Self::ProfileTarget { .. } => {
                "list installed packages with `pohunek plugin list` and pass --digest"
            }
            Self::ProfileWrite { .. } => {
                "check the agents directory and run `pohunek plugin profile list`"
            }
        }
    }

    /// Error class: every plugin-command failure is a usage or configuration mistake.
    #[expect(
        clippy::unused_self,
        reason = "kept beside code() and hint() so every variant can classify itself later"
    )]
    pub(crate) fn class(&self) -> ErrorClass {
        ErrorClass::Configuration
    }
}

/// Reject any effective host other than this machine.
///
/// # Errors
///
/// Returns [`Error::LocalOnly`] for a remote host.
pub(crate) fn ensure_local(host: &str) -> Result<(), Error> {
    if is_local_host(host) {
        Ok(())
    } else {
        Err(Error::LocalOnly {
            host: host.to_owned(),
        })
    }
}

/// How a selector that matches several installed versions is resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ambiguity {
    /// Several matches are an error.
    Reject,
    /// Several matches resolve to the selected version when exactly one is.
    PreferSelected,
}

/// Resolve `selector` against the installed packages.
fn resolve<'a>(
    packages: &'a [PackageInfo],
    selector: &SelectorArgs,
    ambiguity: Ambiguity,
) -> Result<&'a PackageInfo, Error> {
    let package = selector.package.to_string();
    let same_id: Vec<&PackageInfo> = packages
        .iter()
        .filter(|info| info.package.id == selector.package)
        .collect();
    if same_id.is_empty() {
        return Err(Error::NotInstalled { package });
    }
    let matching: Vec<&PackageInfo> = same_id
        .into_iter()
        .filter(|info| {
            selector
                .version
                .as_ref()
                .is_none_or(|version| &info.package.version == version)
                && selector
                    .digest
                    .as_ref()
                    .is_none_or(|digest| digest.matches(&info.digest))
        })
        .collect();
    match matching.as_slice() {
        [] => Err(Error::NoMatchingVersion { package }),
        [only] => Ok(only),
        several => {
            if ambiguity == Ambiguity::PreferSelected {
                let mut selected = several.iter().filter(|info| info.selected);
                if let (Some(chosen), None) = (selected.next(), selected.next()) {
                    return Ok(chosen);
                }
            }
            let candidates: BTreeSet<String> = several
                .iter()
                .map(|info| format!("{} ({})", info.package.version, short_digest(&info.digest)))
                .collect();
            Err(Error::Ambiguous {
                package,
                candidates: candidates.into_iter().collect::<Vec<_>>().join(", "),
            })
        }
    }
}

/// What an install-like command does beyond validating the package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ingest {
    /// `install`: select only when no other version of the id is installed.
    Install { enable: bool },
    /// `link`: always disabled and unselected.
    Link,
    /// `update`: enabled and selected over the installed versions.
    Update,
}

impl Ingest {
    fn verb(self) -> &'static str {
        match self {
            Self::Install { .. } => "installing",
            Self::Link => "linking",
            Self::Update => "updating to",
        }
    }
}

/// Where an install-like command reads the package from.
#[derive(Debug, Clone)]
enum Source {
    /// A package archive with the trust that authorizes it.
    Archive { path: String, trust: PackageTrust },
    /// A developer package directory.
    Directory(String),
}

/// Run one `pohunek plugin` subcommand and return its exit code.
///
/// # Errors
///
/// Returns [`CliError`] when the host is not local, a source path is invalid,
/// a selector does not resolve, consent is missing, or the daemon fails.
pub(crate) async fn run(action: Action, host: &str) -> Result<ExitCode, CliError> {
    ensure_local(host)?;
    match action {
        Action::Install(args) => run_install(&args).await?,
        Action::Link(args) => run_link(&args).await?,
        Action::Update(args) => run_update(&args).await?,
        Action::List { json } => {
            let mut client = connect().await?;
            let result = list(&mut client).await?;
            print_output(json, &result, || render_list(&result))?;
        }
        Action::Inspect { selector, json } => run_inspect(&selector, json).await?,
        Action::Select { selector, json } => {
            let mut client = connect().await?;
            let listed = list(&mut client).await?;
            let digest = resolve(&listed.packages, &selector, Ambiguity::Reject)?
                .digest
                .clone();
            let result = client
                .call::<method::PackageSelect>(PackageSelectParams { digest })
                .await?;
            print_output(json, &result, || render_change("Selected", &result))?;
        }
        Action::Enable { selector, json } => {
            let result = set_enabled(&selector, true).await?;
            print_output(json, &result, || render_change("Enabled", &result))?;
        }
        Action::Disable { selector, json } => {
            let result = set_enabled(&selector, false).await?;
            print_output(json, &result, || render_change("Disabled", &result))?;
        }
        Action::Uninstall(args) => run_uninstall(&args).await?,
        Action::Profile { action } => plugin_profile::run(action).await?,
        Action::Doctor { package, json } => {
            let mut client = connect().await?;
            let result = client
                .call::<method::PackageDoctor>(PackageDoctorParams { package })
                .await?;
            print_output(json, &result, || render_doctor(&result))?;
            if !result.findings.is_empty() {
                return Ok(ExitCode::FAILURE);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn run_install(args: &InstallArgs) -> Result<(), CliError> {
    let source = Source::Archive {
        path: canonical_path(&args.archive, "file", Path::is_file)?,
        trust: trust(&args.trust)?,
    };
    let ingest = Ingest::Install {
        enable: !args.no_enable,
    };
    let mut client = connect().await?;
    ingest_package(&mut client, &source, ingest, None, args.yes, args.json).await
}

async fn run_link(args: &LinkArgs) -> Result<(), CliError> {
    let source = Source::Directory(canonical_path(&args.directory, "directory", Path::is_dir)?);
    let mut client = connect().await?;
    ingest_package(
        &mut client,
        &source,
        Ingest::Link,
        None,
        args.yes,
        args.json,
    )
    .await
}

async fn run_update(args: &UpdateArgs) -> Result<(), CliError> {
    let source = Source::Archive {
        path: canonical_path(&args.archive, "file", Path::is_file)?,
        trust: trust(&args.trust)?,
    };
    let mut client = connect().await?;
    ingest_package(
        &mut client,
        &source,
        Ingest::Update,
        Some(&args.package),
        args.yes,
        args.json,
    )
    .await
}

async fn run_inspect(selector: &SelectorArgs, json: bool) -> Result<(), CliError> {
    let mut client = connect().await?;
    let listed = list(&mut client).await?;
    let info = resolve(&listed.packages, selector, Ambiguity::PreferSelected)?;
    let result = client
        .call::<method::PackageInspect>(PackageInspectParams {
            digest: info.digest.clone(),
        })
        .await?;
    print_output(json, &result, || render_inspect(&result))
}

async fn run_uninstall(args: &UninstallArgs) -> Result<(), CliError> {
    let mut client = connect().await?;
    let listed = list(&mut client).await?;
    let info = resolve(&listed.packages, &args.selector, Ambiguity::Reject)?;
    if !args.yes {
        if !args.json {
            print!("{}", render_uninstall_review(info));
        }
        return Err(Error::ConsentRequired {
            verb: "uninstalling",
            summary: summary(&info.package.id, &info.package.version, &info.digest),
        }
        .into());
    }
    let result = client
        .call::<method::PackageUninstall>(PackageUninstallParams {
            digest: info.digest.clone(),
            remove_modified: args.remove_modified,
        })
        .await?;
    print_output(args.json, &result, || render_uninstall(info, &result))
}

async fn connect() -> Result<Client, CliError> {
    let paths = Paths::resolve()?;
    Client::connect(LOCAL_HOST, &paths).await
}

pub(super) async fn list(client: &mut Client) -> Result<PackageListResult, CliError> {
    client.call::<method::PackageList>(()).await
}

async fn set_enabled(
    selector: &SelectorArgs,
    enabled: bool,
) -> Result<PackageChangeResult, CliError> {
    let mut client = connect().await?;
    let listed = list(&mut client).await?;
    let digest = resolve(&listed.packages, selector, Ambiguity::Reject)?
        .digest
        .clone();
    client
        .call::<method::PackageSetEnabled>(PackageSetEnabledParams { digest, enabled })
        .await
}

/// Canonicalize a user-typed path and check its kind.
fn canonical_path(
    path: &Path,
    expected: &'static str,
    kind_matches: fn(&Path) -> bool,
) -> Result<String, CliError> {
    let typed = path.display().to_string();
    let invalid = || {
        CliError::from(Error::InvalidSource {
            path: typed.clone(),
            expected,
        })
    };
    let canonical = std::fs::canonicalize(path).map_err(|_unreadable| invalid())?;
    if !kind_matches(&canonical) {
        return Err(invalid());
    }
    canonical
        .into_os_string()
        .into_string()
        .map_err(|_unreadable| invalid())
}

/// Convert the trust flags to the wire trust, canonicalizing a catalog path.
fn trust(args: &TrustArgs) -> Result<PackageTrust, CliError> {
    match (&args.sha256, &args.catalog) {
        (Some(digest), None) => Ok(PackageTrust::ExplicitDigest {
            digest: digest.clone(),
        }),
        (None, Some(catalog)) => Ok(PackageTrust::Catalog {
            catalog_path: canonical_path(catalog, "file", Path::is_file)?,
        }),
        // The clap group guarantees exactly one of the two.
        _ => unreachable!("clap enforces exactly one of --sha256 and --catalog"),
    }
}

/// Validate a package with a dry run, ask for consent, then install it.
async fn ingest_package(
    client: &mut Client,
    source: &Source,
    ingest: Ingest,
    expected_id: Option<&PackageId>,
    yes: bool,
    json: bool,
) -> Result<(), CliError> {
    let installed = list(client).await?;
    if let Some(expected) = expected_id {
        if !installed
            .packages
            .iter()
            .any(|info| &info.package.id == expected)
        {
            return Err(Error::NotInstalled {
                package: expected.to_string(),
            }
            .into());
        }
    }
    let enable = match ingest {
        Ingest::Install { enable } => enable,
        Ingest::Link => false,
        Ingest::Update => true,
    };
    let preview = send_ingest(client, source, enable, false, true).await?;
    let id = &preview.package.package.id;
    if let Some(expected) = expected_id {
        if id != expected {
            return Err(Error::UpdateIdMismatch {
                expected: expected.to_string(),
                found: id.to_string(),
            }
            .into());
        }
    }
    let another_version_installed = installed
        .packages
        .iter()
        .any(|info| &info.package.id == id && info.digest != preview.package.digest);
    let select = match ingest {
        Ingest::Install { .. } => !another_version_installed,
        Ingest::Link => false,
        Ingest::Update => true,
    };
    if !yes {
        if !json {
            print!("{}", render_install_review(&preview, enable, select));
        }
        return Err(Error::ConsentRequired {
            verb: ingest.verb(),
            summary: summary(
                id,
                &preview.package.package.version,
                &preview.package.digest,
            ),
        }
        .into());
    }
    let mut result = send_ingest(client, source, enable, select, false).await?;
    if ingest == Ingest::Update {
        converge_update(client, &mut result).await?;
    }
    print_output(json, &result, || render_install_result(&result, ingest))?;
    Ok(())
}

/// Makes an already recorded version enabled and selected, as `update`
/// promises.
///
/// The daemon keeps the recorded enabled and selected state of a package that
/// is already installed, so an update to a version that was installed earlier
/// and left disabled or unselected would otherwise report success and change
/// nothing.
async fn converge_update(
    client: &mut Client,
    result: &mut PackageInstallResult,
) -> Result<(), CliError> {
    let digest = result.package.digest.clone();
    if !result.package.enabled {
        let change = client
            .call::<method::PackageSetEnabled>(PackageSetEnabledParams {
                digest: digest.clone(),
                enabled: true,
            })
            .await?;
        result.package = change.package;
        result.reloaded = change.reloaded;
    }
    if !result.package.selected {
        let change = client
            .call::<method::PackageSelect>(PackageSelectParams { digest })
            .await?;
        result.package = change.package;
        result.reloaded = change.reloaded;
    }
    Ok(())
}

async fn send_ingest(
    client: &mut Client,
    source: &Source,
    enable: bool,
    select: bool,
    dry_run: bool,
) -> Result<PackageInstallResult, CliError> {
    match source {
        Source::Archive { path, trust } => {
            client
                .call::<method::PackageInstall>(PackageInstallParams {
                    archive_path: path.clone(),
                    trust: trust.clone(),
                    enable,
                    select,
                    dry_run,
                })
                .await
        }
        Source::Directory(directory) => {
            client
                .call::<method::PackageLink>(PackageLinkParams {
                    directory: directory.clone(),
                    dry_run,
                })
                .await
        }
    }
}

/// Print `value` as the JSON envelope, or the human text `human` builds.
pub(super) fn print_output<T, F>(json: bool, value: &T, human: F) -> Result<(), CliError>
where
    T: serde::Serialize,
    F: FnOnce() -> String,
{
    if json {
        print!("{}", render_json(value)?);
    } else {
        print!("{}", human());
    }
    Ok(())
}

/// First hex characters of a digest, with the `sha256:` prefix.
pub(super) fn short_digest(digest: &PackageDigest) -> String {
    let hex = digest
        .as_str()
        .strip_prefix(DIGEST_PREFIX)
        .unwrap_or(digest.as_str());
    let end = hex
        .char_indices()
        .nth(DIGEST_PREFIX_HEX_CHARS)
        .map_or(hex.len(), |(index, _)| index);
    format!("{DIGEST_PREFIX}{}", &hex[..end])
}

pub(super) fn summary(id: &PackageId, version: &PackageVersion, digest: &PackageDigest) -> String {
    format!("{id} {version} ({})", short_digest(digest))
}

/// Escape control characters of text that came from a package.
///
/// Package descriptors are untrusted until the owner consents, so their text
/// never reaches the terminal raw.
pub(super) fn sanitize(text: &str) -> String {
    text.chars()
        .flat_map(|character| {
            if character.is_control() {
                character.escape_default().collect::<Vec<_>>()
            } else {
                vec![character]
            }
        })
        .collect()
}

fn origin_label(origin: PackageOrigin) -> &'static str {
    match origin {
        PackageOrigin::Official => "official (signed catalog)",
        PackageOrigin::ExplicitDigest => "explicit digest (third party, pinned by the owner)",
        PackageOrigin::Link => "link (developer directory copy)",
    }
}

fn origin_column(origin: PackageOrigin) -> &'static str {
    match origin {
        PackageOrigin::Official => "official",
        PackageOrigin::ExplicitDigest => "digest",
        PackageOrigin::Link => "link",
    }
}

fn fault_label(fault: PackageFault) -> &'static str {
    match fault {
        PackageFault::RootMissing => "root missing",
        PackageFault::RootModified => "root modified",
        PackageFault::RootUnsafe => "root unsafe",
        PackageFault::RootUnreadable => "root unreadable",
        PackageFault::DescriptorMissing => "descriptor missing",
        PackageFault::DescriptorInvalid => "descriptor invalid",
        PackageFault::IdentityMismatch => "identity mismatch",
        PackageFault::RuntimeNotClaimable => "runtime not claimable",
        PackageFault::RuntimeConflict => "runtime conflict",
    }
}

pub(super) fn state_label(info: &PackageInfo) -> String {
    let mut parts = vec![if info.enabled { "enabled" } else { "disabled" }];
    if info.selected {
        parts.push("selected");
    }
    if info.referenced {
        parts.push("pinned");
    }
    parts.join(" ")
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn render_list(result: &PackageListResult) -> String {
    if result.packages.is_empty() {
        return "No runtime packages are installed.\n".to_owned();
    }
    let mut packages: Vec<&PackageInfo> = result.packages.iter().collect();
    packages.sort_by(|left, right| {
        (
            &left.package.id,
            left.installed_at_unix_seconds,
            &left.digest,
        )
            .cmp(&(
                &right.package.id,
                right.installed_at_unix_seconds,
                &right.digest,
            ))
    });
    let rows: Vec<[String; 6]> = packages
        .iter()
        .map(|info| {
            [
                info.package.id.to_string(),
                info.package.version.to_string(),
                short_digest(&info.digest),
                info.runtime_id
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), ToString::to_string),
                origin_column(info.origin).to_owned(),
                info.fault.map_or_else(
                    || state_label(info),
                    |fault| format!("{}; {}", state_label(info), fault_label(fault)),
                ),
            ]
        })
        .collect();
    render_table(
        ["PACKAGE", "VERSION", "DIGEST", "RUNTIME", "ORIGIN", "STATE"],
        &rows,
    )
}

/// Render rows as a left-aligned table whose last column is unpadded.
pub(super) fn render_table<const N: usize>(header: [&str; N], rows: &[[String; N]]) -> String {
    let mut widths = header.map(str::len);
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut output = String::new();
    let mut push_row = |cells: Vec<&str>| {
        let last = cells.len().saturating_sub(1);
        for (index, cell) in cells.iter().enumerate() {
            if index == last {
                output.push_str(cell);
            } else {
                let _ = write!(output, "{cell:<width$}  ", width = widths[index]);
            }
        }
        output.push('\n');
    };
    push_row(header.to_vec());
    for row in rows {
        push_row(row.iter().map(String::as_str).collect());
    }
    output
}

fn render_package_details(output: &mut String, info: &PackageInfo) {
    let _ = writeln!(
        output,
        "Package:     {} {}",
        info.package.id, info.package.version
    );
    let _ = writeln!(output, "Digest:      {}", info.digest);
    let _ = writeln!(output, "Trust:       {}", origin_label(info.origin));
}

/// An argument template as the owner reads it; `{reference}` marks the slot
/// core fills with the native session reference.
fn template_text(tokens: Option<&[String]>) -> String {
    tokens.map_or_else(|| "none".to_owned(), |tokens| format!("{tokens:?}"))
}

fn render_runtime_details(output: &mut String, runtime: &PackageRuntimeInfo) {
    let _ = writeln!(output, "Runtime:     {}", runtime.runtime_id);
    let _ = writeln!(output, "Name:        {}", sanitize(&runtime.display_name));
    let _ = writeln!(output, "Program:     {:?}", runtime.program);
    let _ = writeln!(output, "Arguments:   {:?}", runtime.args);
    let _ = writeln!(
        output,
        "Launch adds: {}",
        template_text(runtime.launch_args.as_deref())
    );
    let _ = writeln!(output, "Resume:      {}", yes_no(runtime.resumable));
    let _ = writeln!(
        output,
        "  arguments: {}",
        template_text(runtime.resume_args.as_deref())
    );
    let _ = writeln!(output, "Fork:        {}", yes_no(runtime.forkable));
    let _ = writeln!(
        output,
        "  arguments: {}",
        template_text(runtime.fork_args.as_deref())
    );
    let _ = writeln!(
        output,
        "First prompt: {}",
        if runtime.prompt_argument {
            "appended to the arguments"
        } else {
            "typed into the terminal"
        }
    );
    let _ = writeln!(
        output,
        "Version probe: {}",
        runtime
            .version_probe
            .as_deref()
            .map_or_else(|| "none".to_owned(), sanitize)
    );
    let _ = writeln!(
        output,
        "Integration: {}",
        runtime
            .integration_handler
            .as_deref()
            .map_or_else(|| "none".to_owned(), sanitize)
    );
}

fn render_inspect(result: &PackageInspectResult) -> String {
    let info = &result.package;
    let mut output = String::new();
    render_package_details(&mut output, info);
    let _ = writeln!(output, "State:       {}", state_label(info));
    let _ = writeln!(
        output,
        "Health:      {}",
        info.fault.map_or("ok", fault_label)
    );
    match &result.runtime {
        Some(runtime) => render_runtime_details(&mut output, runtime),
        None => {
            let _ = writeln!(output, "Runtime:     descriptor unavailable");
        }
    }
    output
}

/// The review printed before an install-like command asks for consent.
fn render_install_review(preview: &PackageInstallResult, enable: bool, select: bool) -> String {
    let mut output = String::from("The package declares:\n");
    render_package_details(&mut output, &preview.package);
    render_runtime_details(&mut output, &preview.runtime);
    let _ = writeln!(output, "Enabled:     {} after install", yes_no(enable));
    let _ = writeln!(output, "Selected:    {} after install", yes_no(select));
    if preview.status == PackageInstallStatus::AlreadyInstalled {
        let _ = writeln!(output, "Note:        this archive is already installed");
    }
    output
}

fn render_install_result(result: &PackageInstallResult, ingest: Ingest) -> String {
    let info = &result.package;
    let what = summary(&info.package.id, &info.package.version, &info.digest);
    let mut output = match result.status {
        PackageInstallStatus::Preview => format!("Validated {what}; nothing was changed.\n"),
        PackageInstallStatus::Installed => format!("Installed {what}.\n"),
        PackageInstallStatus::AlreadyInstalled => format!("{what} was already installed.\n"),
        PackageInstallStatus::RootRestored => {
            format!("{what} was already recorded; its missing root was restored.\n")
        }
    };
    let _ = writeln!(output, "State: {}", state_label(info));
    if !info.enabled {
        let _ = writeln!(
            output,
            "Fresh sessions cannot launch it yet; enable it with `pohunek plugin enable {} --digest {}`.",
            info.package.id,
            short_digest(&info.digest).trim_start_matches(DIGEST_PREFIX)
        );
    }
    if !info.selected {
        let _ = writeln!(
            output,
            "Bare requests do not resolve to it yet; select it with `pohunek plugin select {} --digest {}`.",
            info.package.id,
            short_digest(&info.digest).trim_start_matches(DIGEST_PREFIX)
        );
    }
    if ingest == Ingest::Link {
        let _ = writeln!(
            output,
            "A linked package is installed disabled and unselected."
        );
    }
    if !result.reloaded {
        let _ = writeln!(output, "The daemon did not reload its runtime registry.");
    }
    output
}

fn render_change(verb: &str, result: &PackageChangeResult) -> String {
    let info = &result.package;
    let mut output = format!(
        "{verb} {}.\nState: {}\n",
        summary(&info.package.id, &info.package.version, &info.digest),
        state_label(info)
    );
    if verb == "Disabled" {
        output.push_str("Live sessions keep running; fresh launches are blocked.\n");
    }
    if !result.reloaded {
        output.push_str("The daemon did not reload its runtime registry.\n");
    }
    output
}

fn render_uninstall_review(info: &PackageInfo) -> String {
    let mut output = String::from("The package to remove:\n");
    render_package_details(&mut output, info);
    let _ = writeln!(output, "State:       {}", state_label(info));
    if let Some(fault) = info.fault {
        let _ = writeln!(output, "Health:      {}", fault_label(fault));
    }
    output
}

fn render_uninstall(info: &PackageInfo, result: &PackageUninstallResult) -> String {
    let mut output = format!(
        "Uninstalled {}.\n",
        summary(&info.package.id, &info.package.version, &result.digest)
    );
    if !result.reloaded {
        output.push_str("The daemon did not reload its runtime registry.\n");
    }
    output
}

fn render_doctor(result: &PackageDoctorResult) -> String {
    if result.findings.is_empty() {
        return format!(
            "No problems found (registry generation {}).\n",
            result.generation
        );
    }
    let rows: Vec<[String; 4]> = result
        .findings
        .iter()
        .map(|finding| {
            let kind = match finding.kind {
                PackageFindingKind::Fault => finding.fault.map_or("fault", fault_label).to_owned(),
                PackageFindingKind::UnregisteredRoot => "unregistered root".to_owned(),
                PackageFindingKind::PinnedNotInstalled => "pinned but not installed".to_owned(),
            };
            [
                kind,
                short_digest(&finding.digest),
                finding.package.as_ref().map_or_else(
                    || "-".to_owned(),
                    |package| format!("{} {}", package.id, package.version),
                ),
                if finding.referenced { "pinned" } else { "-" }.to_owned(),
            ]
        })
        .collect();
    let mut output = render_table(["FINDING", "DIGEST", "PACKAGE", "PINNED"], &rows);
    let _ = writeln!(
        output,
        "{} finding(s) (registry generation {}).",
        result.findings.len(),
        result.generation
    );
    output
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;
    use protocol::{PackageFinding, RuntimeId};

    use super::*;
    use crate::{Cli, Commands};

    const DIGEST_A: &str =
        "sha256:aaaaaaaaaaaa1111111111111111111111111111111111111111111111111111";
    const DIGEST_B: &str =
        "sha256:aaaaaaaaaaaa2222222222222222222222222222222222222222222222222222";
    const DIGEST_C: &str =
        "sha256:bbbbbbbbbbbb3333333333333333333333333333333333333333333333333333";

    fn info(id: &str, version: &str, digest: &str, selected: bool) -> PackageInfo {
        PackageInfo {
            digest: PackageDigest::parse(digest).expect("digest"),
            package: protocol::PackageIdentity {
                id: PackageId::parse(id).expect("id"),
                version: PackageVersion::parse(version).expect("version"),
            },
            origin: PackageOrigin::ExplicitDigest,
            enabled: true,
            selected,
            installed_at_unix_seconds: 1,
            runtime_id: Some(RuntimeId::parse("pi").expect("runtime")),
            fault: None,
            referenced: false,
        }
    }

    fn installed() -> Vec<PackageInfo> {
        vec![
            info("acme.pi", "1.0.0", DIGEST_A, false),
            info("acme.pi", "2.0.0", DIGEST_B, true),
            info("acme.other", "1.0.0", DIGEST_C, true),
        ]
    }

    fn selector(package: &str, digest: Option<&str>, version: Option<&str>) -> SelectorArgs {
        SelectorArgs {
            package: PackageId::parse(package).expect("id"),
            digest: digest.map(|value| value.parse().expect("digest selector")),
            version: version.map(|value| PackageVersion::parse(value).expect("version")),
        }
    }

    fn parse(args: &[&str]) -> Result<Action, clap::Error> {
        let mut words = vec!["pohunek", "plugin"];
        words.extend_from_slice(args);
        match Cli::try_parse_from(words)?.command {
            Commands::Plugin { action } => Ok(action),
            other => panic!("unexpected command {other:?}"),
        }
    }

    fn parse_err(args: &[&str]) -> clap::error::ErrorKind {
        parse(args).expect_err("must not parse").kind()
    }

    #[test]
    fn list_and_doctor_parse() {
        assert!(matches!(
            parse(&["list", "--json"]).expect("list"),
            Action::List { json: true }
        ));
        match parse(&["doctor", "acme.pi"]).expect("doctor") {
            Action::Doctor { package, json } => {
                assert_eq!(package.map(|id| id.to_string()).as_deref(), Some("acme.pi"));
                assert!(!json);
            }
            other => panic!("unexpected {other:?}"),
        }
        match parse(&["doctor"]).expect("doctor") {
            Action::Doctor { package, .. } => assert!(package.is_none()),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn selector_commands_parse_with_narrowing() {
        for name in ["inspect", "select", "enable", "disable"] {
            let action = parse(&[
                name,
                "acme.pi",
                "--digest",
                "aaaaaaaaaaaa",
                "--version",
                "1.0.0",
                "--json",
            ])
            .unwrap_or_else(|error| panic!("{name}: {error}"));
            assert!(action.wants_json(), "{name}");
            let selector = match action {
                Action::Inspect { selector, .. }
                | Action::Select { selector, .. }
                | Action::Enable { selector, .. }
                | Action::Disable { selector, .. } => selector,
                other => panic!("unexpected {other:?}"),
            };
            assert_eq!(selector.package.as_str(), "acme.pi");
            assert_eq!(
                selector.digest,
                Some(DigestSelector::Prefix("aaaaaaaaaaaa".to_owned()))
            );
            assert_eq!(
                selector.version.map(|v| v.to_string()).as_deref(),
                Some("1.0.0")
            );
        }
    }

    #[test]
    fn install_requires_exactly_one_trust_source() {
        let digest = DIGEST_A;
        match parse(&[
            "install",
            "pkg.tar",
            "--sha256",
            digest,
            "--yes",
            "--no-enable",
        ])
        .expect("install")
        {
            Action::Install(args) => {
                assert!(args.yes && args.no_enable && !args.json);
                assert_eq!(args.trust.sha256.expect("digest").as_str(), digest);
                assert!(args.trust.catalog.is_none());
            }
            other => panic!("unexpected {other:?}"),
        }
        match parse(&["install", "pkg.tar", "--catalog", "catalog.json"]).expect("install") {
            Action::Install(args) => {
                assert_eq!(args.trust.catalog, Some(PathBuf::from("catalog.json")));
                assert!(!args.yes && !args.no_enable);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            parse_err(&["install", "pkg.tar"]),
            clap::error::ErrorKind::MissingRequiredArgument
        );
        assert_eq!(
            parse_err(&[
                "install",
                "pkg.tar",
                "--sha256",
                digest,
                "--catalog",
                "catalog.json"
            ]),
            clap::error::ErrorKind::ArgumentConflict
        );
    }

    #[test]
    fn sha256_accepts_the_bare_hex_of_sha256sum_and_rejects_malformed_digests() {
        let bare = DIGEST_A.trim_start_matches("sha256:");
        match parse(&["install", "p", "--sha256", bare]).expect("bare hex") {
            Action::Install(args) => {
                assert_eq!(args.trust.sha256.expect("digest").as_str(), DIGEST_A);
            }
            other => panic!("unexpected {other:?}"),
        }
        for bad in ["sha256:abc", "XYZ", "", &DIGEST_A.to_uppercase()] {
            assert_eq!(
                parse_err(&["install", "p", "--sha256", bad]),
                clap::error::ErrorKind::ValueValidation,
                "{bad}"
            );
        }
    }

    #[test]
    fn link_update_and_uninstall_parse() {
        match parse(&["link", "./dir", "--yes", "--json"]).expect("link") {
            Action::Link(args) => assert!(args.yes && args.json),
            other => panic!("unexpected {other:?}"),
        }
        match parse(&["update", "acme.pi", "new.tar", "--sha256", DIGEST_B]).expect("update") {
            Action::Update(args) => {
                assert_eq!(args.package.as_str(), "acme.pi");
                assert_eq!(args.archive, PathBuf::from("new.tar"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            parse_err(&["update", "acme.pi", "new.tar"]),
            clap::error::ErrorKind::MissingRequiredArgument
        );
        match parse(&[
            "uninstall",
            "acme.pi",
            "--version",
            "1.0.0",
            "--remove-modified",
            "--yes",
        ])
        .expect("uninstall")
        {
            Action::Uninstall(args) => assert!(args.remove_modified && args.yes),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn invalid_package_ids_and_short_digest_prefixes_are_usage_errors() {
        assert_eq!(
            parse_err(&["enable", "Not A Package"]),
            clap::error::ErrorKind::ValueValidation
        );
        assert_eq!(
            parse_err(&["enable", "acme.pi", "--digest", "aaaaaaaaaaa"]),
            clap::error::ErrorKind::ValueValidation
        );
        assert_eq!(
            parse_err(&["enable", "acme.pi", "--digest", "zzzzzzzzzzzz"]),
            clap::error::ErrorKind::ValueValidation
        );
        assert_eq!(
            parse_err(&["enable"]),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn digest_selector_accepts_full_prefixed_and_unprefixed_forms() {
        let hex = DIGEST_A.trim_start_matches("sha256:");
        assert_eq!(
            DIGEST_A.parse::<DigestSelector>(),
            Ok(DigestSelector::Full(
                PackageDigest::parse(DIGEST_A).expect("digest")
            ))
        );
        assert_eq!(
            hex.parse::<DigestSelector>(),
            Ok(DigestSelector::Full(
                PackageDigest::parse(DIGEST_A).expect("digest")
            ))
        );
        assert_eq!(
            "sha256:AAAAAAAAAAAA".parse::<DigestSelector>(),
            Ok(DigestSelector::Prefix("aaaaaaaaaaaa".to_owned()))
        );
        let too_long = format!("{hex}0");
        too_long
            .parse::<DigestSelector>()
            .expect_err("65 hex characters");
    }

    #[test]
    fn resolve_reports_unknown_ids_and_unmatched_narrowing() {
        let packages = installed();
        assert!(matches!(
            resolve(&packages, &selector("nope", None, None), Ambiguity::Reject),
            Err(Error::NotInstalled { .. })
        ));
        assert!(matches!(
            resolve(
                &packages,
                &selector("acme.pi", None, Some("9.9.9")),
                Ambiguity::Reject
            ),
            Err(Error::NoMatchingVersion { .. })
        ));
        assert!(matches!(
            resolve(
                &packages,
                &selector("acme.pi", Some("cccccccccccc"), None),
                Ambiguity::Reject
            ),
            Err(Error::NoMatchingVersion { .. })
        ));
    }

    #[test]
    fn resolve_narrows_by_version_digest_prefix_and_full_digest() {
        let packages = installed();
        let by_version = resolve(
            &packages,
            &selector("acme.pi", None, Some("1.0.0")),
            Ambiguity::Reject,
        )
        .expect("version");
        assert_eq!(by_version.digest.as_str(), DIGEST_A);
        let by_prefix = resolve(
            &packages,
            &selector("acme.pi", Some("aaaaaaaaaaaa2"), None),
            Ambiguity::Reject,
        )
        .expect("prefix");
        assert_eq!(by_prefix.digest.as_str(), DIGEST_B);
        let by_full = resolve(
            &packages,
            &selector("acme.pi", Some(DIGEST_A), None),
            Ambiguity::Reject,
        )
        .expect("full digest");
        assert_eq!(by_full.package.version.as_str(), "1.0.0");
        let sole = resolve(
            &packages,
            &selector("acme.other", None, None),
            Ambiguity::Reject,
        )
        .expect("single version");
        assert_eq!(sole.digest.as_str(), DIGEST_C);
    }

    #[test]
    fn ambiguous_prefix_never_guesses_and_lists_candidates() {
        let packages = installed();
        let error = resolve(
            &packages,
            &selector("acme.pi", Some("aaaaaaaaaaaa"), None),
            Ambiguity::Reject,
        )
        .expect_err("two versions share the prefix");
        match error {
            Error::Ambiguous { candidates, .. } => {
                assert!(
                    candidates.contains("1.0.0 (sha256:aaaaaaaaaaaa)"),
                    "{candidates}"
                );
                assert!(
                    candidates.contains("2.0.0 (sha256:aaaaaaaaaaaa)"),
                    "{candidates}"
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn inspect_prefers_the_selected_version_but_mutations_do_not() {
        let packages = installed();
        let chosen = resolve(
            &packages,
            &selector("acme.pi", None, None),
            Ambiguity::PreferSelected,
        )
        .expect("selected version");
        assert_eq!(chosen.package.version.as_str(), "2.0.0");
        assert!(matches!(
            resolve(
                &packages,
                &selector("acme.pi", None, None),
                Ambiguity::Reject
            ),
            Err(Error::Ambiguous { .. })
        ));
        let none_selected = vec![
            info("acme.pi", "1.0.0", DIGEST_A, false),
            info("acme.pi", "2.0.0", DIGEST_B, false),
        ];
        assert!(matches!(
            resolve(
                &none_selected,
                &selector("acme.pi", None, None),
                Ambiguity::PreferSelected
            ),
            Err(Error::Ambiguous { .. })
        ));
    }

    #[test]
    fn remote_hosts_are_rejected_and_local_forms_accepted() {
        ensure_local(LOCAL_HOST).expect("local");
        ensure_local("").expect("empty host means local");
        let error = ensure_local("build-box").expect_err("remote host");
        assert_eq!(error.code(), "plugin_local_only");
        assert!(error.to_string().contains("this machine only"));
    }

    #[test]
    fn errors_map_to_stable_codes_with_hints() {
        let errors = [
            Error::LocalOnly { host: "h".into() },
            Error::InvalidSource {
                path: "p".into(),
                expected: "file",
            },
            Error::ConsentRequired {
                verb: "installing",
                summary: "s".into(),
            },
            Error::NotInstalled {
                package: "p".into(),
            },
            Error::NoMatchingVersion {
                package: "p".into(),
            },
            Error::Ambiguous {
                package: "p".into(),
                candidates: "c".into(),
            },
            Error::UpdateIdMismatch {
                expected: "a".into(),
                found: "b".into(),
            },
            Error::ProfileDirectory { detail: "d".into() },
            Error::ProfileNotFound { name: "n".into() },
            Error::ProfileUnusable {
                name: "n".into(),
                detail: "d".into(),
            },
            Error::ProfileBaseBuiltin {
                name: "n".into(),
                base: "b".into(),
            },
            Error::ProfileTarget {
                name: "n".into(),
                detail: "d".into(),
            },
            Error::ProfileWrite {
                name: "n".into(),
                detail: "d".into(),
            },
        ];
        for error in &errors {
            assert!(!error.code().is_empty());
            assert!(!error.hint().is_empty());
            assert_eq!(error.class(), ErrorClass::Configuration);
        }
        assert_eq!(errors[2].code(), "consent_required");
    }

    #[test]
    fn list_table_abbreviates_digests_and_marks_state() {
        let mut faulty = info("acme.bad", "1.0.0", DIGEST_C, false);
        faulty.enabled = false;
        faulty.fault = Some(PackageFault::RootModified);
        faulty.runtime_id = None;
        let mut packages = installed();
        packages.push(faulty);
        let text = render_list(&PackageListResult {
            generation: 3,
            packages,
        });
        assert!(text.starts_with("PACKAGE"), "{text}");
        assert!(text.contains("sha256:aaaaaaaaaaaa"), "{text}");
        assert!(!text.contains(DIGEST_A), "{text}");
        assert!(text.contains("enabled selected"), "{text}");
        assert!(text.contains("disabled; root modified"), "{text}");
        assert_eq!(
            render_list(&PackageListResult {
                generation: 0,
                packages: Vec::new()
            }),
            "No runtime packages are installed.\n"
        );
    }

    fn runtime(name: &str) -> PackageRuntimeInfo {
        PackageRuntimeInfo {
            runtime_id: RuntimeId::parse("pi").expect("runtime"),
            display_name: name.to_owned(),
            program: "bin/pi".to_owned(),
            args: vec!["--mode".to_owned(), "rpc".to_owned()],
            launch_args: Some(vec![
                "-c".to_owned(),
                "run \u{1b}[31m".to_owned(),
                "{reference}".to_owned(),
            ]),
            resume_args: Some(vec!["--session".to_owned(), "{reference}".to_owned()]),
            fork_args: None,
            prompt_argument: true,
            version_probe: Some("probe".to_owned()),
            resumable: true,
            forkable: false,
            integration_handler: None,
        }
    }

    #[test]
    fn install_review_shows_what_the_owner_consents_to_and_escapes_controls() {
        let preview = PackageInstallResult {
            status: PackageInstallStatus::Preview,
            package: info("acme.pi", "1.0.0", DIGEST_A, false),
            runtime: runtime("Pi\u{1b}[31m red"),
            reloaded: false,
        };
        let text = render_install_review(&preview, true, false);
        for needle in [
            "Launch adds: [\"-c\", \"run \\u{1b}[31m\", \"{reference}\"]",
            "arguments: [\"--session\", \"{reference}\"]",
            "First prompt: appended to the arguments",
            "Version probe: probe",
            "acme.pi 1.0.0",
            DIGEST_A,
            "explicit digest",
            "Runtime:     pi",
            "\"bin/pi\"",
            "[\"--mode\", \"rpc\"]",
            "Resume:      yes",
            "Fork:        no",
            "Integration: none",
            "Enabled:     yes after install",
            "Selected:    no after install",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in {text}");
        }
        assert!(
            !text.contains('\u{1b}'),
            "raw escape reached the terminal: {text:?}"
        );
        assert!(text.contains("\\u{1b}"), "{text}");
    }

    #[test]
    fn install_result_explains_how_to_enable_a_disabled_package() {
        let mut package = info("acme.pi", "1.0.0", DIGEST_A, false);
        package.enabled = false;
        let result = PackageInstallResult {
            status: PackageInstallStatus::Installed,
            package,
            runtime: runtime("Pi"),
            reloaded: true,
        };
        let text = render_install_result(&result, Ingest::Link);
        assert!(text.contains("Installed acme.pi 1.0.0"), "{text}");
        assert!(
            text.contains("pohunek plugin enable acme.pi --digest aaaaaaaaaaaa"),
            "{text}"
        );
    }

    #[test]
    fn doctor_renders_findings_or_a_clean_report() {
        assert!(render_doctor(&PackageDoctorResult {
            generation: 7,
            findings: Vec::new()
        })
        .starts_with("No problems found"));
        let text = render_doctor(&PackageDoctorResult {
            generation: 7,
            findings: vec![
                PackageFinding {
                    kind: PackageFindingKind::Fault,
                    digest: PackageDigest::parse(DIGEST_A).expect("digest"),
                    package: Some(protocol::PackageIdentity {
                        id: PackageId::parse("acme.pi").expect("id"),
                        version: PackageVersion::parse("1.0.0").expect("version"),
                    }),
                    fault: Some(PackageFault::RootModified),
                    referenced: true,
                },
                PackageFinding {
                    kind: PackageFindingKind::UnregisteredRoot,
                    digest: PackageDigest::parse(DIGEST_B).expect("digest"),
                    package: None,
                    fault: None,
                    referenced: false,
                },
            ],
        });
        assert!(text.contains("root modified"), "{text}");
        assert!(text.contains("unregistered root"), "{text}");
        assert!(text.contains("2 finding(s)"), "{text}");
    }

    #[test]
    fn disable_output_states_the_live_session_semantics() {
        let result = PackageChangeResult {
            package: info("acme.pi", "1.0.0", DIGEST_A, true),
            reloaded: true,
        };
        let text = render_change("Disabled", &result);
        assert!(text.contains("Live sessions keep running"), "{text}");
        assert!(!render_change("Enabled", &result).contains("Live sessions"));
    }

    #[test]
    fn canonical_path_checks_existence_and_kind_before_any_request() {
        let dir = pohunek_test_support::tempdir().expect("tempdir");
        let file = dir.path().join("pkg.tar");
        std::fs::write(&file, b"x").expect("write archive");
        let canonical = canonical_path(&file, "file", Path::is_file).expect("file");
        assert!(Path::new(&canonical).is_absolute());
        canonical_path(dir.path(), "file", Path::is_file).expect_err("a directory is not a file");
        canonical_path(&file, "directory", Path::is_dir).expect_err("a file is not a directory");
        let missing = dir.path().join("missing");
        let error = canonical_path(&missing, "file", Path::is_file).expect_err("missing");
        assert!(error.to_string().contains("missing"), "{error}");
    }

    #[test]
    fn uninstall_review_lists_the_pin_state() {
        let mut package = info("acme.pi", "1.0.0", DIGEST_A, true);
        package.referenced = true;
        let text = render_uninstall_review(&package);
        assert!(text.contains("pinned"), "{text}");
        assert!(text.contains("acme.pi 1.0.0"), "{text}");
    }
}
