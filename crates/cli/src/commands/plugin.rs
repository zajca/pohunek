//! `pohunek plugin` runtime package lifecycle commands.
//!
//! Every subcommand is a request to the daemon on this machine's local control
//! socket: installing a package extends the owner's launch authority on that
//! machine, so the commands reject any other effective host before any I/O.
//! Installation and removal follow one consent pattern: the daemon validates
//! the package in a dry run, the CLI prints what it declares, and nothing
//! changes until the owner repeats the command with `--yes`.

// Rust guideline compliant 2026-10-18

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;
use std::time::Duration;

use clap::{Args, Subcommand, ValueHint};
use protocol::{
    method, CommandExposure, CommandInfo, ErrorClass, PackageChangeResult, PackageDigest,
    PackageDoctorParams, PackageDoctorResult, PackageFault, PackageFindingKind, PackageId,
    PackageInfo, PackageInspectParams, PackageInspectResult, PackageInstallParams,
    PackageInstallResult, PackageInstallStatus, PackageLinkParams, PackageListResult,
    PackageOrigin, PackageRuntimeInfo, PackageSelectParams, PackageSelectionBlockReason,
    PackageSetEnabledParams, PackageTrust, PackageUninstallParams, PackageUninstallResult,
    PackageVersion, RuntimeId,
};

use crate::client::Client;
use crate::commands::plugin_index::{self, PackageRef, Resolved};
use crate::commands::plugin_profile::{self, ProfileAction};
use crate::commands::render_json;
use crate::error::CliError;
use crate::paths::Paths;
use crate::target::{is_local_host, LOCAL_HOST};

mod release;

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

/// File name suffix of a package archive.
const ARCHIVE_SUFFIX: &str = ".tar.zst";

/// Archive validation can exceed 5 s on a loaded host; keep each response bounded.
const INSTALL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

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

    /// Install a package archive, or an official package by id.
    ///
    /// The daemon validates the archive and this command prints what the
    /// package declares, including the program it would launch as you. Nothing
    /// is installed until you repeat the command with `--yes`.
    ///
    /// Trust is explicit: a third-party archive is authorized only by the
    /// digest you supply with `--sha256`; `--catalog` and `--index` authorize
    /// official packages through a signed catalog or plugin index file.
    ///
    /// Without a trust flag, SOURCE is `<package-id>[@<version>]`: this
    /// command downloads the official plugin index and the archive it lists,
    /// and the daemon verifies the index signature and the archive digest
    /// before anything is installed. The review names the exact version, and
    /// the command to repeat pins it.
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
    /// Authorize the archive as an official package through this signed
    /// plugin index file.
    #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath)]
    index: Option<PathBuf>,
}

/// How an install authorizes its source: at most one flag, because without
/// one the source is a package id resolved through the plugin index.
#[derive(Debug, Clone, Args)]
#[group(required = false, multiple = false)]
pub(crate) struct InstallTrustArgs {
    /// Authorize the archive by its SHA-256 digest (`sha256:<64 hex>`, or the
    /// bare hex `sha256sum` prints). Third-party packages are trusted only by
    /// a digest the owner supplies; this never authorizes an official runtime.
    #[arg(long, value_name = "DIGEST", value_parser = parse_archive_digest)]
    sha256: Option<PackageDigest>,
    /// Authorize the archive as an official package through this signed
    /// catalog file.
    #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath)]
    catalog: Option<PathBuf>,
    /// Authorize the archive as an official package through this signed
    /// plugin index file.
    #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath)]
    index: Option<PathBuf>,
}

/// Arguments of `pohunek plugin install`.
#[derive(Debug, Clone, Args)]
pub(crate) struct InstallArgs {
    /// A package archive to install (with `--sha256`, `--catalog` or
    /// `--index`), or `<package-id>[@<version>]` to install through the
    /// plugin index.
    #[arg(
        value_name = "SOURCE",
        value_hint = ValueHint::AnyPath,
        required_unless_present = "release",
        conflicts_with = "release"
    )]
    source: Option<String>,
    /// Install the official package of `--runtime` from this extracted
    /// daemon archive. With a local daemon the install goes through it; on a
    /// host without one, the archive is verified and its own `pohunekd`
    /// installs the package once, without installing a service.
    #[arg(
        long,
        value_name = "DIR",
        value_hint = ValueHint::DirPath,
        requires = "runtime",
        conflicts_with_all = ["sha256", "catalog", "index", "index_url", "no_enable"]
    )]
    release: Option<PathBuf>,
    /// The runtime whose official package `--release` installs (for example
    /// `hermes`).
    #[arg(long, value_name = "ID", requires = "release", value_parser = parse_runtime_id)]
    runtime: Option<RuntimeId>,
    #[command(flatten)]
    trust: InstallTrustArgs,
    /// Plugin index to resolve a package id through, instead of the official
    /// one. Only `https` URLs and `http` to this machine are fetched.
    #[arg(
        long,
        value_name = "URL",
        conflicts_with_all = ["sha256", "catalog", "index"]
    )]
    index_url: Option<String>,
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

/// Parse a runtime id for `--runtime`.
fn parse_runtime_id(value: &str) -> Result<RuntimeId, String> {
    RuntimeId::parse(value).map_err(|_malformed| format!("`{value}` is not a runtime id"))
}

/// Parse an archive digest: `sha256:<64 hex>` or the bare 64 hex characters.
pub(super) fn parse_archive_digest(value: &str) -> Result<PackageDigest, String> {
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

    /// A package id (and version) given as the install source does not parse.
    #[error("{source_text:?} is not a package id or `<package-id>@<version>`: {detail}")]
    PackageRefInvalid {
        /// The source as typed.
        source_text: String,
        /// Why the id or version is invalid.
        detail: String,
    },

    /// The source names a file but no trust flag was given.
    #[error("{source_text:?} names a file, so a trust flag is required")]
    TrustRequired {
        /// The source as typed.
        source_text: String,
    },

    /// A URL is not `https` (or `http` to this machine) or carries credentials.
    #[error("refusing to fetch {url}: only https URLs are allowed")]
    IndexUrlRefused {
        /// The URL with its path and query removed.
        url: String,
    },

    /// A download exceeds its size limit.
    #[error("the {what} download is larger than {limit} bytes")]
    DownloadTooLarge {
        /// `index` or `archive`.
        what: &'static str,
        /// The limit in bytes.
        limit: u64,
    },

    /// The plugin index could not be fetched.
    #[error("the plugin index cannot be fetched: {detail}")]
    IndexUnavailable {
        /// What failed, without the URL.
        detail: String,
    },

    /// The archive the index lists could not be fetched.
    #[error("the package archive cannot be fetched: {detail}")]
    ArchiveUnavailable {
        /// What failed, without the URL.
        detail: String,
    },

    /// The daemon cannot authorize installs through a plugin index.
    #[error("the daemon does not support installs through a plugin index")]
    IndexUnsupported,

    /// The daemon authorizes runtime packages through a plugin index but not
    /// context packages (index schema 2).
    #[error("the daemon does not support installing context packages through a plugin index")]
    IndexContextUnsupported,

    /// The fetched index document is not a valid plugin index.
    #[error("the plugin index document is invalid: {detail}")]
    IndexInvalid {
        /// The parser's reason.
        detail: String,
    },

    /// The fetched index has expired.
    #[error("the plugin index has expired")]
    IndexExpired,

    /// `plugin profile show` named no agent the host inventories.
    #[error("no agent profile named {name} on this host")]
    ProfileNotFound {
        /// The requested profile name.
        name: String,
    },

    /// No entry of the index fits the id on this host and core version.
    #[error("the plugin index has no {package} for this platform and core version")]
    IndexPackageNotFound {
        /// The requested package id.
        package: String,
    },

    /// The requested version is not among the matching entries.
    #[error(
        "the plugin index has no version {version} of {package} for this platform and core version"
    )]
    IndexVersionNotFound {
        /// The requested package id.
        package: String,
        /// The requested version.
        version: String,
    },

    /// The downloaded archive is not the one the index lists.
    #[error("the downloaded archive has digest {actual}, the index lists {expected}")]
    ArchiveDigestMismatch {
        /// The digest the index lists.
        expected: String,
        /// The digest of the downloaded bytes.
        actual: String,
    },

    /// This host has no platform name in the plugin index.
    #[error("this platform has no plugin index entries")]
    PlatformUnsupported,

    /// The downloaded files could not be staged for the daemon.
    #[error("the downloaded files cannot be staged: {detail}")]
    StagingFailed {
        /// The failure kind, without a path.
        detail: String,
    },

    /// A by-id install needs the owner's consent; the repeat pins the version.
    #[error("{verb} {summary} needs your consent; nothing was changed")]
    ConsentRequiredPinned {
        /// Gerund describing the refused action.
        verb: &'static str,
        /// Package id, version, and abbreviated digest.
        summary: String,
        /// The command that installs exactly the reviewed version.
        command: String,
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

    /// The update would select an older official version than the selected one.
    #[error(
        "updating {package} to {offered} would replace the selected official version {installed}; nothing was changed; to roll back on purpose, {rollback}`pohunek plugin select {package} --digest {digest}`"
    )]
    UpdateDowngrade {
        /// The package id being updated.
        package: String,
        /// The selected official version the update would replace.
        installed: String,
        /// The older version the archive holds.
        offered: String,
        /// Abbreviated digest of the archive, accepted by `--digest`.
        digest: String,
        /// The step before the select: empty when the older version is
        /// installed already, else its install, which leaves it unselected.
        rollback: &'static str,
    },

    /// The update would replace a selected official version whose order
    /// against the offered one cannot be proven, because one is not semantic.
    #[error(
        "updating {package} to {offered} cannot be ordered against the selected official version {installed}, because a version is not semantic; nothing was changed; the order cannot be proven"
    )]
    UpdateVersionUnordered {
        /// The package id being updated.
        package: String,
        /// The selected official version the update would replace.
        installed: String,
        /// The version the archive holds.
        offered: String,
    },

    /// The agents directory is missing its safety properties or unreadable.
    #[error("the agent profiles directory cannot be used: {detail}")]
    ProfileDirectory {
        /// What is wrong, without file content.
        detail: String,
    },

    /// The daemon predates `package.create_profile`.
    #[error("the local daemon is too old for `plugin profile create`")]
    ProfileCreateUnsupported,

    /// The migration target does not resolve to one usable package.
    #[error("profile {name}: {detail}")]
    ProfileTarget {
        /// The profile name.
        name: String,
        /// Why no single target resolves.
        detail: String,
    },
}

impl Error {
    /// Stable wire error code.
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::LocalOnly { .. } => "plugin_local_only",
            Self::InvalidSource { .. } | Self::PackageRefInvalid { .. } => "plugin_source_invalid",
            Self::TrustRequired { .. } => "plugin_trust_required",
            Self::IndexUrlRefused { .. } => "plugin_index_url_refused",
            Self::DownloadTooLarge { .. } => "plugin_download_too_large",
            Self::IndexUnavailable { .. } => "plugin_index_unavailable",
            Self::ArchiveUnavailable { .. } => "plugin_archive_unavailable",
            Self::IndexUnsupported => "plugin_index_unsupported",
            Self::IndexContextUnsupported => "plugin_index_context_unsupported",
            Self::IndexInvalid { .. } => "plugin_index_invalid",
            Self::IndexExpired => "plugin_index_expired",
            Self::ProfileNotFound { .. } => "plugin_profile_not_found",
            Self::IndexPackageNotFound { .. } => "plugin_index_package_not_found",
            Self::IndexVersionNotFound { .. } => "plugin_index_version_not_found",
            Self::ArchiveDigestMismatch { .. } => "plugin_archive_digest_mismatch",
            Self::PlatformUnsupported => "plugin_platform_unsupported",
            Self::StagingFailed { .. } => "plugin_staging_failed",
            Self::ConsentRequired { .. } | Self::ConsentRequiredPinned { .. } => "consent_required",
            Self::NotInstalled { .. } | Self::NoMatchingVersion { .. } => "package_not_installed",
            Self::Ambiguous { .. } => "plugin_selector_ambiguous",
            Self::UpdateIdMismatch { .. } => "plugin_update_id_mismatch",
            Self::UpdateDowngrade { .. } => "plugin_update_downgrade",
            Self::UpdateVersionUnordered { .. } => "plugin_update_version_unordered",
            Self::ProfileDirectory { .. } => "profile_directory_unusable",
            Self::ProfileTarget { .. } => "profile_target_invalid",
            Self::ProfileCreateUnsupported => "profile_create_unsupported",
        }
    }

    /// Recovery hint shown beneath the error.
    pub(crate) fn hint(&self) -> Cow<'static, str> {
        if let Self::ConsentRequiredPinned { command, .. } = self {
            return Cow::Owned(format!(
                "review the package details (omit --json to see them), then run `{command}`"
            ));
        }
        Cow::Borrowed(self.fixed_hint())
    }

    /// Recovery hint that does not depend on the error's fields.
    fn fixed_hint(&self) -> &'static str {
        match self {
            Self::LocalOnly { .. } => {
                "run the command on the machine that runs the daemon, without --host"
            }
            Self::InvalidSource { .. } => "pass an existing path of the expected kind",
            Self::PackageRefInvalid { .. } => {
                "pass `<package-id>` or `<package-id>@<version>`, or a trust flag with an archive path"
            }
            Self::TrustRequired { .. } => {
                "pass --sha256 <digest>, --catalog <file> or --index <file> to install an archive file"
            }
            Self::IndexUrlRefused { .. } => {
                "use an https URL; plain http is accepted only for 127.0.0.1 and [::1]"
            }
            Self::DownloadTooLarge { .. } => {
                "the server sent more than the package format allows; nothing was installed"
            }
            Self::IndexUnavailable { .. } | Self::ArchiveUnavailable { .. } => {
                "check the network and the URL, then retry; nothing was sent to the daemon"
            }
            Self::IndexUnsupported => {
                "the daemon is older than 0.40 or this host has no plugin index channel file beside the daemon executable; upgrade the daemon or install from an archive with --sha256 or --catalog"
            }
            Self::IndexContextUnsupported => {
                "the daemon predates context packages in the plugin index (0.42); upgrade the daemon or install the context package from its archive with --sha256"
            }
            Self::IndexInvalid { .. } => {
                "the index is malformed; retry later or report it to the index maintainers"
            }
            Self::IndexExpired => {
                "the index is out of date; retry later or report it to the index maintainers"
            }
            Self::ProfileNotFound { .. } => {
                "list the host's profiles with `pohunek plugin profile list --all`"
            }
            Self::IndexPackageNotFound { .. } => {
                "check the package id; the index may not list it for this platform and core version"
            }
            Self::IndexVersionNotFound { .. } => {
                "omit the version to take the newest one the index lists for this host"
            }
            Self::ArchiveDigestMismatch { .. } => {
                "the download does not match the index; nothing was installed, retry later"
            }
            Self::PlatformUnsupported => "plugin indexes list packages for Linux and macOS only",
            Self::StagingFailed { .. } => {
                "check that the temporary directory ($TMPDIR) is writable and has free space"
            }
            Self::ConsentRequired { .. } => {
                "review the package details (omit --json to see them), then repeat the command with --yes"
            }
            Self::ConsentRequiredPinned { .. } => {
                "review the package details, then repeat the pinned command with --yes"
            }
            Self::NotInstalled { .. } => "list installed packages with `pohunek plugin list`",
            Self::NoMatchingVersion { .. } => {
                "list installed versions with `pohunek plugin list` and adjust --digest or --version"
            }
            Self::Ambiguous { .. } => "narrow the selection with --digest or --version",
            Self::UpdateIdMismatch { .. } => {
                "name the package id the archive declares, or use `pohunek plugin install`"
            }
            Self::UpdateDowngrade { .. } => {
                "list installed versions with `pohunek plugin list`, and keep the selected version unless a rollback is intended"
            }
            Self::UpdateVersionUnordered { .. } => {
                "install the archive with `pohunek plugin install <archive> ...` (another version stays selected), then run `pohunek plugin select <package> --digest <digest>`"
            }
            Self::ProfileDirectory { .. } => {
                "the agents directory must be owned by you and not group- or world-writable"
            }
            Self::ProfileCreateUnsupported => "run `pohunek service upgrade`, then repeat the command",
            Self::ProfileTarget { .. } => {
                "list installed packages with `pohunek plugin list` and pass --digest"
            }
        }
    }

    /// Whether the failure is a mistake in how the command was invoked, which
    /// exits with the usage status.
    pub(crate) fn is_usage(&self) -> bool {
        matches!(
            self,
            Self::PackageRefInvalid { .. } | Self::TrustRequired { .. }
        )
    }

    /// Error class: a failed or oversized download of the index or an archive is
    /// a transport failure; every other plugin-command failure is a usage or
    /// configuration mistake.
    pub(crate) fn class(&self) -> ErrorClass {
        match self {
            Self::IndexUnavailable { .. }
            | Self::ArchiveUnavailable { .. }
            | Self::DownloadTooLarge { .. } => ErrorClass::Transport,
            _ => ErrorClass::Configuration,
        }
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
    let Some(source_text) = args.source.as_deref() else {
        return release::run(args).await;
    };
    let ingest = Ingest::Install {
        enable: !args.no_enable,
    };
    let Some(trust) = install_trust(&args.trust)? else {
        return run_install_by_id(args, source_text, ingest).await;
    };
    let source = Source::Archive {
        path: canonical_path(Path::new(source_text), "file", Path::is_file)?,
        trust,
    };
    let mut client = connect_ingest().await?;
    require_index_support_for(&mut client, &source).await?;
    ingest_package(
        &mut client,
        &source,
        ingest,
        None,
        None,
        args.yes,
        args.json,
    )
    .await
}

/// Install `<package-id>[@<version>]`: fetch the index and the archive it
/// lists, then hand both to the daemon, which verifies them.
async fn run_install_by_id(
    args: &InstallArgs,
    source_text: &str,
    ingest: Ingest,
) -> Result<(), CliError> {
    // A forgotten trust flag must not turn into a network lookup: an archive
    // file name such as `muse.tar.zst` is also a valid package id.
    if source_text.ends_with(ARCHIVE_SUFFIX) || std::fs::symlink_metadata(source_text).is_ok() {
        return Err(Error::TrustRequired {
            source_text: source_text.to_owned(),
        }
        .into());
    }
    let request = PackageRef::parse(source_text)?;
    let mut client = connect_ingest().await?;
    require_index_support(&mut client).await?;
    if request
        .id
        .as_str()
        .starts_with(package::OFFICIAL_CONTEXT_PACKAGE_PREFIX)
    {
        require_index_context_support(&mut client).await?;
    }
    let staged = plugin_index::stage(&request, args.index_url.as_deref()).await?;
    let source = Source::Archive {
        path: staged.archive_path.clone(),
        trust: PackageTrust::Index {
            index_path: staged.index_path.clone(),
        },
    };
    let pin = Pin {
        command: pinned_command(&staged.resolved, args),
        resolved: staged.resolved.clone(),
    };
    ingest_package(
        &mut client,
        &source,
        ingest,
        None,
        Some(&pin),
        args.yes,
        args.json,
    )
    .await
}

/// The reviewed version of a by-id install and the command that installs it.
struct Pin {
    resolved: Resolved,
    command: String,
}

/// The command that installs exactly `resolved`: it keeps the index the
/// review used and the enable choice, so the repeat cannot resolve elsewhere.
fn pinned_command(resolved: &Resolved, args: &InstallArgs) -> String {
    let mut command = format!("pohunek plugin install {} --yes", resolved.pinned());
    if args.no_enable {
        command.push_str(" --no-enable");
    }
    if let Some(url) = &args.index_url {
        let _ = write!(command, " --index-url '{}'", url.replace('\'', "'\\''"));
    }
    command
}

/// Refuse before any fetch or dry run when the daemon cannot take an index
/// install, which a daemon older than this CLI would otherwise reject only
/// after the network work.
async fn require_index_support(client: &mut Client) -> Result<(), CliError> {
    if client
        .host_inspect_with(protocol::HostInspectParams {
            skip_runtime_probes: Some(true),
        })
        .await?
        .package_install_index_supported
    {
        Ok(())
    } else {
        Err(Error::IndexUnsupported.into())
    }
}

/// Refuse before any fetch when the daemon cannot authorize a context package
/// through the index.
async fn require_index_context_support(client: &mut Client) -> Result<(), CliError> {
    if client
        .host_inspect_with(protocol::HostInspectParams {
            skip_runtime_probes: Some(true),
        })
        .await?
        .package_install_index_context_supported
    {
        Ok(())
    } else {
        Err(Error::IndexContextUnsupported.into())
    }
}

/// [`require_index_support`] for a source that is authorized by an index.
async fn require_index_support_for(client: &mut Client, source: &Source) -> Result<(), CliError> {
    match source {
        Source::Archive {
            trust: PackageTrust::Index { .. },
            ..
        } => require_index_support(client).await,
        _ => Ok(()),
    }
}

async fn run_link(args: &LinkArgs) -> Result<(), CliError> {
    let source = Source::Directory(canonical_path(&args.directory, "directory", Path::is_dir)?);
    let mut client = connect().await?;
    ingest_package(
        &mut client,
        &source,
        Ingest::Link,
        None,
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
    let mut client = connect_ingest().await?;
    require_index_support_for(&mut client, &source).await?;
    ingest_package(
        &mut client,
        &source,
        Ingest::Update,
        Some(&args.package),
        None,
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

/// Connects archive install/update commands with their longer response budget.
async fn connect_ingest() -> Result<Client, CliError> {
    let paths = Paths::resolve()?;
    Client::connect_with_request_timeout(LOCAL_HOST, &paths, INSTALL_RESPONSE_TIMEOUT).await
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

/// Convert the trust flags to the wire trust, canonicalizing a document path.
fn trust(args: &TrustArgs) -> Result<PackageTrust, CliError> {
    wire_trust(
        args.sha256.as_ref(),
        args.catalog.as_deref(),
        args.index.as_deref(),
    )?
    // The clap group guarantees one of the three.
    .ok_or_else(|| unreachable!("clap enforces one of --sha256, --catalog and --index"))
}

/// The trust of an install, or `None` when no flag was given and the source
/// is a package id.
fn install_trust(args: &InstallTrustArgs) -> Result<Option<PackageTrust>, CliError> {
    wire_trust(
        args.sha256.as_ref(),
        args.catalog.as_deref(),
        args.index.as_deref(),
    )
}

/// The wire trust for at most one of the three flags.
fn wire_trust(
    sha256: Option<&PackageDigest>,
    catalog: Option<&Path>,
    index: Option<&Path>,
) -> Result<Option<PackageTrust>, CliError> {
    match (sha256, catalog, index) {
        (Some(digest), None, None) => Ok(Some(PackageTrust::ExplicitDigest {
            digest: digest.clone(),
        })),
        (None, Some(catalog), None) => Ok(Some(PackageTrust::Catalog {
            catalog_path: canonical_path(catalog, "file", Path::is_file)?,
        })),
        (None, None, Some(index)) => Ok(Some(PackageTrust::Index {
            index_path: canonical_path(index, "file", Path::is_file)?,
        })),
        (None, None, None) => Ok(None),
        // The clap groups allow at most one of the three.
        _ => unreachable!("clap enforces at most one of --sha256, --catalog and --index"),
    }
}

/// Validate a package with a dry run, ask for consent, then install it.
async fn ingest_package(
    client: &mut Client,
    source: &Source,
    ingest: Ingest,
    expected_id: Option<&PackageId>,
    pin: Option<&Pin>,
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
    let accept_commands = host_shows_commands(client).await?;
    let preview = send_ingest(client, source, (enable, false, true), accept_commands).await?;
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
    if ingest == Ingest::Update {
        ensure_not_official_downgrade(&installed, &preview.package)?;
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
            let recorded = installed
                .packages
                .iter()
                .find(|info| info.digest == preview.package.digest);
            let selected = selection_after(ingest, select, &preview.package, recorded);
            if let Some(pin) = pin {
                println!(
                    "Resolved:    {} (digest {}) from the plugin index",
                    pin.resolved.pinned(),
                    pin.resolved.digest.as_str()
                );
            }
            print!("{}", render_install_review(&preview, enable, selected));
        }
        let verb = ingest.verb();
        let summary = summary(
            id,
            &preview.package.package.version,
            &preview.package.digest,
        );
        return Err(match pin {
            Some(pin) => Error::ConsentRequiredPinned {
                verb,
                summary,
                command: pin.command.clone(),
            },
            None => Error::ConsentRequired { verb, summary },
        }
        .into());
    }
    let mut result = send_ingest(client, source, (enable, select, false), accept_commands).await?;
    if ingest == Ingest::Update {
        converge_update(client, &mut result).await?;
    }
    print_output(json, &result, || render_install_result(&result, ingest))?;
    Ok(())
}

/// Whether the downgrade guard treats a package of `origin` as official: an
/// origin this client does not know keeps the guard engaged.
const fn guarded_as_official(origin: PackageOrigin) -> bool {
    matches!(origin, PackageOrigin::Official | PackageOrigin::Unknown)
}

/// Refuses an official update whose version is older than the selected
/// official version of the same package, or whose order cannot be proven.
///
/// The daemon accepts any signed catalog at or above its high-water sequence,
/// so an older official version stays installable; only the update flow
/// reads the semantic order. An archive counts as official when the catalog
/// listed it or its digest is already recorded as official, because an
/// explicit digest carries no catalog origin. A package whose origin this
/// client does not know counts as official, so the guard stays engaged. Build
/// metadata does not order versions. Rolling back on purpose is `pohunek plugin select`.
fn ensure_not_official_downgrade(
    installed: &PackageListResult,
    preview: &PackageInfo,
) -> Result<(), Error> {
    let recorded_official = installed
        .packages
        .iter()
        .any(|info| info.digest == preview.digest && guarded_as_official(info.origin));
    if !guarded_as_official(preview.origin) && !recorded_official {
        return Ok(());
    }
    let Some(selected) = installed.packages.iter().find(|info| {
        info.selected && guarded_as_official(info.origin) && info.package.id == preview.package.id
    }) else {
        return Ok(());
    };
    // Re-selecting the version already selected is no change, so its order
    // is not needed even when the version string does not parse.
    if selected.digest == preview.digest {
        return Ok(());
    }
    let (Ok(offered), Ok(current)) = (
        semver::Version::parse(preview.package.version.as_str()),
        semver::Version::parse(selected.package.version.as_str()),
    ) else {
        return Err(Error::UpdateVersionUnordered {
            package: preview.package.id.to_string(),
            installed: selected.package.version.to_string(),
            offered: preview.package.version.to_string(),
        });
    };
    if !offered.cmp_precedence(&current).is_lt() {
        return Ok(());
    }
    let recorded = installed
        .packages
        .iter()
        .any(|info| info.digest == preview.digest);
    Err(Error::UpdateDowngrade {
        package: preview.package.id.to_string(),
        installed: selected.package.version.to_string(),
        offered: preview.package.version.to_string(),
        digest: short_digest(&preview.digest),
        rollback: if recorded {
            "run "
        } else {
            "install the archive with `pohunek plugin install` (another version stays selected), then run "
        },
    })
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

/// Whether the daemon can take the `accept_commands` opt-in, which this CLI
/// then sends because it renders every exported command for consent.
///
/// An older daemon rejects the unknown field, so the opt-in is sent only to a
/// host that advertises it.
async fn host_shows_commands(client: &mut Client) -> Result<bool, CliError> {
    Ok(client
        .host_inspect_with(protocol::HostInspectParams {
            skip_runtime_probes: Some(true),
        })
        .await?
        .package_install_commands_supported)
}

/// Sends the install or link request; `flags` is `(enable, select, dry_run)`.
async fn send_ingest(
    client: &mut Client,
    source: &Source,
    flags: (bool, bool, bool),
    accept_commands: bool,
) -> Result<PackageInstallResult, CliError> {
    let (enable, select, dry_run) = flags;
    match source {
        Source::Archive { path, trust } => {
            client
                .call::<method::PackageInstall>(PackageInstallParams {
                    archive_path: path.clone(),
                    trust: trust.clone(),
                    enable,
                    select,
                    dry_run,
                    accept_commands,
                })
                .await
        }
        Source::Directory(directory) => {
            client
                .call::<method::PackageLink>(PackageLinkParams {
                    directory: directory.clone(),
                    dry_run,
                    accept_commands,
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

/// Escape non-printable characters of text that came from a package.
///
/// Package descriptors are untrusted until the owner consents, so their text
/// never reaches the terminal raw: control, format (such as bidirectional
/// overrides and zero-width characters) and other non-printable characters are
/// escaped as `{:?}` escapes them. Quotes and backslashes stay as written.
pub(super) fn sanitize(text: &str) -> String {
    text.chars()
        .flat_map(|character| {
            if matches!(character, '"' | '\'' | '\\') {
                vec![character]
            } else {
                character.escape_debug().collect::<Vec<_>>()
            }
        })
        .collect()
}

fn origin_label(origin: PackageOrigin) -> &'static str {
    match origin {
        PackageOrigin::Official => "official (signed catalog)",
        PackageOrigin::ExplicitDigest => "explicit digest (third party, pinned by the owner)",
        PackageOrigin::Link => "link (developer directory copy)",
        PackageOrigin::Unknown => "unknown (an origin this client does not know)",
    }
}

fn origin_column(origin: PackageOrigin) -> &'static str {
    match origin {
        PackageOrigin::Official => "official",
        PackageOrigin::ExplicitDigest => "digest",
        PackageOrigin::Link => "link",
        PackageOrigin::Unknown => "unknown",
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
        PackageFault::Unknown => "unknown",
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
    if info.selection_blocked.is_some() {
        parts.push("selection blocked");
    }
    if info.revoked {
        parts.push("revoked");
    }
    parts.join(" ")
}

/// Why the package cannot be selected yet, worded for the owner; `None` when
/// nothing holds it back.
fn selection_note(info: &PackageInfo) -> Option<String> {
    let block = info.selection_blocked.as_ref()?;
    Some(match block.reason {
        PackageSelectionBlockReason::IncompatibleWithRetained => format!(
            "its integration handler or hook schema differs from version {} of the same package, which a session or profile still uses; the package stays installed and unselected until that reference is gone, then select it with `pohunek plugin select`",
            short_digest(&block.retained)
        ),
        PackageSelectionBlockReason::Unknown => format!(
            "the daemon holds it back for a reason this client does not know (version {} of the same package is involved); it stays unselected until the reference is gone",
            short_digest(&block.retained)
        ),
    })
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

/// The commands a package exports as the owner reads them before consenting:
/// each one's program, fixed arguments and who may call it. An exposure this
/// build does not know is shown as the widest one.
fn render_command_details(output: &mut String, commands: &[CommandInfo]) {
    for command in commands {
        let _ = writeln!(output, "Command:     {}", command.name);
        let _ = writeln!(output, "  program:   {:?}", command.program);
        let _ = writeln!(output, "  arguments: {:?}", command.args);
        let _ = writeln!(
            output,
            "  exposure:  {}",
            match command.expose {
                CommandExposure::Local => "local (only the owner's local control socket)",
                CommandExposure::Peers => {
                    "peers (callable by every overlay peer the overlay policy admits)"
                }
                _ => "unknown (treated as callable by every overlay peer)",
            }
        );
    }
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
    let _ = writeln!(
        output,
        "Hook schema: {}",
        runtime
            .hook_schema
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
    if let Some(note) = selection_note(info) {
        let _ = writeln!(output, "Selection:   blocked: {note}");
    }
    match &result.runtime {
        Some(runtime) => render_runtime_details(&mut output, runtime),
        None if result.commands_only => {
            let _ = writeln!(
                output,
                "Runtime:     {}",
                runtimeless_note(&result.commands)
            );
        }
        None => {
            let _ = writeln!(output, "Runtime:     descriptor unavailable");
        }
    }
    render_command_details(&mut output, &result.commands);
    output
}

/// Whether the package is selected after the command succeeds.
///
/// `preview` comes from a dry run that never selects, so the outcome is
/// derived from what the command does: an archive that is already installed
/// keeps its recorded selection under `install` and `link`, `update` selects
/// it unless the selection is held back, and a fresh install selects when
/// `requested` and not held back. `recorded` is the installed package of the
/// archive's digest, if any.
fn selection_after(
    ingest: Ingest,
    requested: bool,
    preview: &PackageInfo,
    recorded: Option<&PackageInfo>,
) -> bool {
    let held_back = preview.selection_blocked.is_some();
    match (recorded, ingest) {
        (Some(_), Ingest::Update) => !held_back,
        (Some(recorded), _) => recorded.selected,
        (None, _) => requested && !held_back,
    }
}

/// What a package without a runtime exports: commands, or else a context
/// directory.
fn runtimeless_note(commands: &[protocol::CommandInfo]) -> &'static str {
    if commands.is_empty() {
        "none (exports a context directory)"
    } else {
        "none (exports commands only)"
    }
}

/// The review printed before an install-like command asks for consent;
/// `selected` is the outcome of [`selection_after`].
fn render_install_review(preview: &PackageInstallResult, enable: bool, selected: bool) -> String {
    let mut output = String::from("The package declares:\n");
    render_package_details(&mut output, &preview.package);
    match &preview.runtime {
        Some(runtime) => render_runtime_details(&mut output, runtime),
        None => {
            let _ = writeln!(
                output,
                "Runtime:     {}",
                runtimeless_note(&preview.commands)
            );
        }
    }
    render_command_details(&mut output, &preview.commands);
    let _ = writeln!(output, "Enabled:     {} after install", yes_no(enable));
    let _ = writeln!(output, "Selected:    {} after install", yes_no(selected));
    if !selected {
        if let Some(note) = selection_note(&preview.package) {
            let _ = writeln!(
                output,
                "Note:        it is installed but not selected: {note}"
            );
        }
    }
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
        PackageInstallStatus::Unknown => {
            format!("{what}: the daemon reported an install status this client does not know.\n")
        }
    };
    let _ = writeln!(output, "State: {}", state_label(info));
    if !info.enabled {
        let _ = writeln!(
            output,
            "{} enable it with `pohunek plugin enable {} --digest {}`.",
            if result.runtime.is_some() {
                "Fresh sessions cannot launch it yet;"
            } else {
                "Its commands are not available yet;"
            },
            info.package.id,
            short_digest(&info.digest).trim_start_matches(DIGEST_PREFIX)
        );
    }
    if let Some(note) = selection_note(info) {
        let _ = writeln!(
            output,
            "Not selected: {note}. Bare requests keep resolving to the selected version."
        );
    } else if !info.selected {
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
                PackageFindingKind::Unknown => "unknown".to_owned(),
                PackageFindingKind::UnregisteredRoot => "unregistered root".to_owned(),
                PackageFindingKind::Revoked => "revoked".to_owned(),
                PackageFindingKind::PinnedNotInstalled => "pinned but not installed".to_owned(),
                PackageFindingKind::SelectionBlocked => finding.blocked_by.as_ref().map_or_else(
                    || "selection blocked".to_owned(),
                    |retained| format!("selection blocked by {}", short_digest(retained)),
                ),
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
    let mut revoked = false;
    for finding in &result.findings {
        if finding.kind != PackageFindingKind::Revoked {
            continue;
        }
        revoked = true;
        if !finding.running_sessions.is_empty() {
            let ids: Vec<&str> = finding
                .running_sessions
                .iter()
                .map(|id| id.0.as_str())
                .collect();
            let _ = writeln!(
                output,
                "Revoked {} still runs: {}",
                short_digest(&finding.digest),
                ids.join(", ")
            );
        }
    }
    if revoked {
        output.push_str(REVOKED_HINT);
    }
    output
}

/// Printed under a doctor table that lists a revoked package.
const REVOKED_HINT: &str = "A revoked package serves no fresh launch, resume or selection; \
sessions already running from it keep running. Select another installed version with \
`pohunek plugin select`, or install a release that carries a new package.\n";

#[cfg(test)]
mod tests {
    use clap::Parser as _;
    use protocol::PackageFinding;

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
            selection_blocked: None,
            revoked: false,
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
    fn install_takes_at_most_one_trust_source_and_update_requires_one() {
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
        match parse(&["install", "pohunek.runtime.idxfix@0.1.0"]).expect("by id") {
            Action::Install(args) => {
                assert!(args.trust.sha256.is_none() && args.trust.catalog.is_none());
                assert!(args.trust.index.is_none());
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            parse_err(&["update", "acme.pi", "pkg.tar"]),
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
            prompt_argument: true,
            version_probe: Some("probe".to_owned()),
            resumable: true,
            forkable: false,
            integration_handler: None,
            hook_schema: None,
        }
    }

    #[test]
    fn install_review_shows_what_the_owner_consents_to_and_escapes_controls() {
        let preview = PackageInstallResult {
            status: PackageInstallStatus::Preview,
            package: info("acme.pi", "1.0.0", DIGEST_A, false),
            runtime: Some(runtime("Pi\u{1b}[31m red")),
            commands: Vec::new(),
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

    fn held_back(version: &str, digest: &str) -> PackageInfo {
        let mut blocked = info("acme.pi", version, digest, false);
        blocked.selection_blocked = Some(protocol::PackageSelectionBlock {
            reason: PackageSelectionBlockReason::IncompatibleWithRetained,
            retained: PackageDigest::parse(DIGEST_A).expect("digest"),
        });
        blocked
    }

    #[test]
    fn a_withheld_selection_is_visible_in_every_view() {
        let blocked = held_back("2.0.0", DIGEST_B);
        assert_eq!(state_label(&blocked), "enabled selection blocked");
        let list = render_list(&PackageListResult {
            generation: 1,
            packages: vec![blocked.clone()],
        });
        assert!(list.contains("enabled selection blocked"), "{list}");

        let mut declared = runtime("Pi");
        declared.integration_handler = Some("codex-hook-v1".to_owned());
        declared.hook_schema = Some("identity-subagent-v1".to_owned());
        let inspect = render_inspect(&PackageInspectResult {
            package: blocked.clone(),
            runtime: Some(declared.clone()),
            commands: Vec::new(),
            commands_only: false,
        });
        assert!(inspect.contains("Selection:   blocked"), "{inspect}");
        assert!(inspect.contains("sha256:aaaaaaaaaaaa"), "{inspect}");
        assert!(
            inspect.contains("Hook schema: identity-subagent-v1"),
            "{inspect}"
        );

        let installed = render_install_result(
            &PackageInstallResult {
                status: PackageInstallStatus::Installed,
                package: blocked.clone(),
                runtime: Some(declared.clone()),
                commands: Vec::new(),
                reloaded: true,
            },
            Ingest::Install { enable: true },
        );
        assert!(
            installed.contains("Not selected: its integration"),
            "{installed}"
        );
        assert!(
            !installed.contains("Bare requests do not resolve to it yet"),
            "{installed}"
        );

        let review = render_install_review(
            &PackageInstallResult {
                status: PackageInstallStatus::Preview,
                package: blocked,
                runtime: Some(declared),
                commands: Vec::new(),
                reloaded: false,
            },
            true,
            false,
        );
        assert!(review.contains("Selected:    no after install"), "{review}");
        assert!(review.contains("installed but not selected"), "{review}");

        let doctor = render_doctor(&PackageDoctorResult {
            generation: 2,
            findings: vec![PackageFinding {
                kind: PackageFindingKind::SelectionBlocked,
                digest: PackageDigest::parse(DIGEST_B).expect("digest"),
                package: None,
                fault: None,
                referenced: false,
                blocked_by: Some(PackageDigest::parse(DIGEST_A).expect("digest")),
                running_sessions: Vec::new(),
            }],
        });
        assert!(
            doctor.contains("selection blocked by sha256:aaaaaaaaaaaa"),
            "{doctor}"
        );
    }

    #[test]
    fn the_review_shows_the_selection_the_command_will_make() {
        let after = |ingest, requested, package: &PackageInfo, recorded: Option<&PackageInfo>| {
            selection_after(ingest, requested, package, recorded)
        };
        let install = Ingest::Install { enable: true };
        // The dry run never selects, so its `selected` says nothing.
        let fresh = info("acme.pi", "2.0.0", DIGEST_B, false);
        assert!(
            after(install, true, &fresh, None),
            "fresh compatible install"
        );
        assert!(after(Ingest::Update, true, &fresh, None), "fresh update");
        assert!(
            !after(install, false, &fresh, None),
            "selection not requested"
        );
        let held = held_back("2.0.0", DIGEST_B);
        assert!(!after(install, true, &held, None), "held back");
        assert!(
            !after(Ingest::Update, true, &held, None),
            "update held back"
        );

        // Reviewer scenario: the archive is installed and unselected, and no
        // other version is installed.
        let unselected = info("acme.pi", "2.0.0", DIGEST_B, false);
        assert!(
            !after(install, true, &fresh, Some(&unselected)),
            "install keeps it"
        );
        assert!(!after(Ingest::Link, false, &fresh, Some(&unselected)));
        assert!(
            after(Ingest::Update, true, &fresh, Some(&unselected)),
            "update selects it"
        );
        assert!(!after(Ingest::Update, true, &held, Some(&unselected)));
        let selected = info("acme.pi", "2.0.0", DIGEST_B, true);
        assert!(after(install, true, &fresh, Some(&selected)));

        let review = |selected| {
            render_install_review(
                &PackageInstallResult {
                    status: PackageInstallStatus::Preview,
                    package: fresh.clone(),
                    runtime: Some(runtime("Pi")),
                    commands: Vec::new(),
                    reloaded: false,
                },
                true,
                selected,
            )
        };
        assert!(review(true).contains("Selected:    yes after install"));
        assert!(review(false).contains("Selected:    no after install"));
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
