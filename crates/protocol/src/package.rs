//! Typed payloads of the runtime package lifecycle methods.
//!
//! The `package.*` methods let the owner inspect and change the runtime
//! packages installed on the host the daemon runs on. They are local-only: the
//! daemon answers them on its local Unix control socket and refuses them on a
//! remote overlay connection, because installing a package extends the
//! owner's launch authority on that machine. A package is addressed by its
//! archive digest; the digest is the content address of the verified root the
//! daemon launches from.
//!
//! The payloads never carry file contents, package paths or secret values: a
//! package root is described by its identity, its trust origin and typed
//! health, never by where it sits on disk.

// Rust guideline compliant 2026-10-04

use serde::{Deserialize, Serialize};

use crate::{ErrorClass, PackageDigest, PackageId, PackageIdentity, ProtocolError, RuntimeId};

/// How an installed package was authorized.
///
/// Mirrors the trust the registry recorded at install time. `explicit_digest`
/// and `link` are local third-party trust; only `official` came through the
/// signed catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "PackageOrigin.ts"))]
pub enum PackageOrigin {
    /// Authorized through the signed official catalog.
    Official,
    /// Installed from an archive the owner pinned by digest.
    ExplicitDigest,
    /// Copied from a developer directory with `package.link`.
    Link,
}

/// Why an installed package cannot serve its runtime.
///
/// Typed and text-free so it is safe to log and to show; the daemon log keeps
/// the precise verification cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "PackageFault.ts"))]
pub enum PackageFault {
    /// The package root is missing from disk.
    RootMissing,
    /// The package root has missing, added or modified content or manifest.
    RootModified,
    /// A root entry has the wrong type, mode, owner or link count.
    RootUnsafe,
    /// The root could not be read.
    RootUnreadable,
    /// The package has no `runtime.toml`.
    DescriptorMissing,
    /// The runtime descriptor is not a valid runtime definition.
    DescriptorInvalid,
    /// The descriptor names another package identity than the registry
    /// recorded.
    IdentityMismatch,
    /// The package claims a runtime id the host does not allow it to serve.
    RuntimeNotClaimable,
    /// Another package claims the same runtime id.
    RuntimeConflict,
}

/// One installed package as the registry records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "PackageInfo.ts"))]
pub struct PackageInfo {
    /// Digest of the package archive, its content address.
    pub digest: PackageDigest,
    /// Package id and exact version.
    pub package: PackageIdentity,
    /// How the package was authorized.
    pub origin: PackageOrigin,
    /// Whether fresh launches may use the package.
    pub enabled: bool,
    /// Whether the package is the one bare requests of its id resolve to.
    pub selected: bool,
    /// Install time, seconds since the Unix epoch.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub installed_at_unix_seconds: u64,
    /// The runtime the package serves; absent when its descriptor cannot be
    /// loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub runtime_id: Option<RuntimeId>,
    /// Why the package cannot serve; absent when its root verified and its
    /// descriptor loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub fault: Option<PackageFault>,
    /// Whether a live, lost or resumable session or a host profile pins the
    /// digest, which keeps it from being uninstalled.
    pub referenced: bool,
}

/// What a package's runtime descriptor declares, for review before and after
/// installation.
///
/// The program, the fixed arguments and every argument template the daemon can
/// add to a launch (reference passing, resume, fork, the first prompt) are
/// exactly what it will run as the owner, so the owner can review all of them
/// before consenting to an install. The descriptor carries no environment
/// values, setup commands or hooks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "PackageRuntimeInfo.ts"))]
pub struct PackageRuntimeInfo {
    /// The runtime the package serves.
    pub runtime_id: RuntimeId,
    /// Presentation name of the runtime.
    pub display_name: String,
    /// Program the runtime launches.
    pub program: String,
    /// Fixed launch arguments.
    pub args: Vec<String>,
    /// Arguments core appends at a fresh launch to pass the generated native
    /// reference; `{reference}` marks where the reference goes. Absent when
    /// the runtime does not assign its reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub launch_args: Option<Vec<String>>,
    /// Arguments of a native resume, with `{reference}` for the reference.
    /// Absent when the runtime cannot resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub resume_args: Option<Vec<String>>,
    /// Arguments of a native fork, with `{reference}` for the reference.
    /// Absent when the runtime cannot fork.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub fork_args: Option<Vec<String>>,
    /// Whether the first prompt of a session is appended to the launch
    /// arguments instead of being typed into the terminal.
    pub prompt_argument: bool,
    /// The core-owned version probe the runtime names, if any. The probe runs
    /// the resolved program with arguments core fixes, never package data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub version_probe: Option<String>,
    /// Whether the runtime supports native resume.
    pub resumable: bool,
    /// Whether the runtime supports native fork.
    pub forkable: bool,
    /// The core-owned integration handler the runtime names, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub integration_handler: Option<String>,
}

/// How an archive is trusted when it is installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "PackageTrust.ts"))]
pub enum PackageTrust {
    /// The owner pins the archive by digest: local third-party trust. It can
    /// never authorize an official runtime alias.
    ExplicitDigest {
        /// The digest the archive must have.
        digest: PackageDigest,
    },
    /// The signed catalog at `catalog_path` authorizes the archive as an
    /// official package.
    Catalog {
        /// Absolute path of the catalog document on the daemon host.
        catalog_path: String,
    },
}

/// Parameters for `package.install`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "PackageInstallParams.ts"))]
pub struct PackageInstallParams {
    /// Absolute path of the package archive on the daemon host.
    pub archive_path: String,
    /// How the archive is authorized.
    pub trust: PackageTrust,
    /// Whether the package starts enabled.
    pub enable: bool,
    /// Whether the package becomes the selected one for its package id.
    pub select: bool,
    /// Validate and report without changing anything.
    pub dry_run: bool,
}

/// Parameters for `package.link`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "PackageLinkParams.ts"))]
pub struct PackageLinkParams {
    /// Absolute path of the package directory on the daemon host. Its content
    /// is copied into content-addressed storage; the daemon never loads the
    /// directory itself. The installed package starts disabled and unselected.
    pub directory: String,
    /// Validate and report without changing anything.
    pub dry_run: bool,
}

/// What an install did to the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "PackageInstallStatus.ts"))]
pub enum PackageInstallStatus {
    /// A dry run: the package validated and nothing changed.
    Preview,
    /// The package was extracted and recorded.
    Installed,
    /// The package was already recorded with a verified root.
    AlreadyInstalled,
    /// The package was recorded but its root was missing and was extracted
    /// again.
    RootRestored,
}

/// Result of `package.install` and `package.link`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "PackageInstallResult.ts"))]
pub struct PackageInstallResult {
    /// What the call did.
    pub status: PackageInstallStatus,
    /// The package as recorded (or as it would be recorded for a preview).
    pub package: PackageInfo,
    /// What the package's runtime descriptor declares.
    pub runtime: PackageRuntimeInfo,
    /// Whether the daemon rebuilt its runtime registry after the change.
    pub reloaded: bool,
}

/// Parameters for `package.set_enabled`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "PackageSetEnabledParams.ts"))]
pub struct PackageSetEnabledParams {
    /// The package to change.
    pub digest: PackageDigest,
    /// The enabled state to record.
    pub enabled: bool,
}

/// Parameters for `package.select`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "PackageSelectParams.ts"))]
pub struct PackageSelectParams {
    /// The installed package bare requests of its id resolve to afterwards.
    pub digest: PackageDigest,
}

/// Result of `package.set_enabled` and `package.select`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "PackageChangeResult.ts"))]
pub struct PackageChangeResult {
    /// The package after the change.
    pub package: PackageInfo,
    /// Whether the daemon rebuilt its runtime registry after the change.
    pub reloaded: bool,
}

/// Parameters for `package.uninstall`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "PackageUninstallParams.ts"))]
pub struct PackageUninstallParams {
    /// The package to remove.
    pub digest: PackageDigest,
    /// Remove a package whose root fails verification. A root that verifies
    /// is refused on this path; uninstall it normally.
    pub remove_modified: bool,
}

/// Result of `package.uninstall`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "PackageUninstallResult.ts"))]
pub struct PackageUninstallResult {
    /// The removed package.
    pub digest: PackageDigest,
    /// Whether the daemon rebuilt its runtime registry after the removal.
    pub reloaded: bool,
}

/// Parameters for `package.bind_profile`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "PackageBindProfileParams.ts"))]
pub struct PackageBindProfileParams {
    /// Name of the host agent profile: its file name under the agents
    /// directory without `.toml`.
    pub profile: String,
    /// The installed package to pin; it must serve the profile's base runtime
    /// and load without a fault. Absent means the selected, enabled package
    /// that serves the base runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub digest: Option<PackageDigest>,
    /// Validate and report without changing the profile.
    pub dry_run: bool,
}

/// What a profile bind did to the profile file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "PackageBindStatus.ts"))]
pub enum PackageBindStatus {
    /// A dry run: the pin would change and nothing was written.
    Preview,
    /// The profile now pins the package.
    Bound,
    /// The profile already pinned the package; nothing was written.
    Unchanged,
}

/// Result of `package.bind_profile`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "PackageBindProfileResult.ts"))]
pub struct PackageBindProfileResult {
    /// What the call did.
    pub status: PackageBindStatus,
    /// The profile name.
    pub profile: String,
    /// The base runtime the profile extends.
    pub base: RuntimeId,
    /// The digest the profile pinned before the call; absent when it pinned
    /// none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub previous: Option<PackageDigest>,
    /// The package the profile pins afterwards (or would pin, for a preview).
    pub package: PackageInfo,
    /// What the package's runtime descriptor declares.
    pub runtime: PackageRuntimeInfo,
    /// Whether the new pin is live. The daemon reads profile files at every
    /// resolution, so a bound profile takes effect at once; a preview and an
    /// unchanged profile report `false`.
    pub reloaded: bool,
}

/// Result of `package.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "PackageListResult.ts"))]
pub struct PackageListResult {
    /// Number of committed registry changes.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub generation: u64,
    /// Installed packages in ascending digest order.
    pub packages: Vec<PackageInfo>,
}

/// Parameters for `package.inspect`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "PackageInspectParams.ts"))]
pub struct PackageInspectParams {
    /// The installed package to inspect.
    pub digest: PackageDigest,
}

/// Result of `package.inspect`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "PackageInspectResult.ts"))]
pub struct PackageInspectResult {
    /// The package record and its health.
    pub package: PackageInfo,
    /// What the descriptor declares; absent when the descriptor cannot be
    /// loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub runtime: Option<PackageRuntimeInfo>,
}

/// Parameters for `package.doctor`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "PackageDoctorParams.ts"))]
pub struct PackageDoctorParams {
    /// Restrict the report to the versions of one package id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub package: Option<PackageId>,
}

/// What a doctor finding is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "PackageFindingKind.ts"))]
pub enum PackageFindingKind {
    /// An installed package cannot serve its runtime; see `fault`.
    Fault,
    /// A package root sits on disk without a registry record. Installing the
    /// same archive again adopts it after verification.
    UnregisteredRoot,
    /// A session or host profile pins a digest the registry does not record.
    PinnedNotInstalled,
}

/// One doctor finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "PackageFinding.ts"))]
pub struct PackageFinding {
    /// What the finding is about.
    pub kind: PackageFindingKind,
    /// The package digest concerned.
    pub digest: PackageDigest,
    /// The recorded identity; absent for an unregistered root or an unknown
    /// pin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub package: Option<PackageIdentity>,
    /// The typed fault of a [`PackageFindingKind::Fault`] finding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub fault: Option<PackageFault>,
    /// Whether a session or host profile pins the digest.
    pub referenced: bool,
}

/// Result of `package.doctor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "PackageDoctorResult.ts"))]
pub struct PackageDoctorResult {
    /// Number of committed registry changes.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub generation: u64,
    /// Every problem found; empty when everything verified.
    pub findings: Vec<PackageFinding>,
}

/// Why a `package.*` request was refused.
///
/// Each kind is one stable wire error code. The messages are fixed text: they
/// carry no path, no archive or package content and no caller-supplied value,
/// so they are safe to log and to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PackageErrorKind {
    /// The request arrived on a remote overlay connection.
    LocalOnly,
    /// The archive is not a valid canonical package archive, breaks a limit
    /// or has another digest than the trust names.
    ArchiveInvalid,
    /// The archive file, package directory or catalog could not be read or is
    /// not an absolute path.
    SourceUnreadable,
    /// The trust does not authorize the archive.
    Untrusted,
    /// The host has no catalog trust anchor, so an official package cannot
    /// be authorized.
    TrustUnavailable,
    /// The package does not support this core version or platform.
    Incompatible,
    /// The package's runtime descriptor is not a valid runtime definition.
    DescriptorInvalid,
    /// The package claims a runtime id it may not serve.
    RuntimeNotClaimable,
    /// Another package or a built-in runtime serves the runtime id.
    RuntimeConflict,
    /// The digest is not installed.
    NotInstalled,
    /// The package id and version are installed from another archive.
    IdentityInstalled,
    /// The digest is installed under another package identity.
    IdentityConflict,
    /// A session or host profile still pins the digest.
    Referenced,
    /// The recorded package root fails verification.
    RootInvalid,
    /// The recorded package root verifies, so it is not removed as modified.
    RootIntact,
    /// Another writer holds the package registry.
    Busy,
    /// The package registry or its storage failed.
    RegistryFailed,
    /// The registry holds the maximum number of packages.
    LimitReached,
    /// The change was committed but the runtime registry was not rebuilt.
    ReloadFailed,
    /// The host has no agent profile of the requested name.
    ProfileNotFound,
    /// The profile cannot be rewritten: it fails the daemon's profile
    /// acceptance rule, is a symbolic or hard link, or is not valid TOML.
    ProfileUnusable,
    /// The profile's base runtime is served by a built-in, so there is no
    /// package to pin.
    ProfileBaseBuiltin,
    /// The requested package cannot be pinned by the profile: it is not
    /// installed, is faulted, does not serve the base runtime, or no unique
    /// selected package serves it.
    ProfileTargetInvalid,
    /// The profile changed while it was being rewritten and was left as found.
    ProfileChanged,
}

impl PackageErrorKind {
    /// The stable wire error code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::LocalOnly => "local_only_method",
            Self::ArchiveInvalid => "package_archive_invalid",
            Self::SourceUnreadable => "package_source_unreadable",
            Self::Untrusted => "package_untrusted",
            Self::TrustUnavailable => "official_trust_unavailable",
            Self::Incompatible => "package_incompatible",
            Self::DescriptorInvalid => "package_descriptor_invalid",
            Self::RuntimeNotClaimable => "package_runtime_not_claimable",
            Self::RuntimeConflict => "package_runtime_conflict",
            Self::NotInstalled => "package_not_installed",
            Self::IdentityInstalled => "package_identity_installed",
            Self::IdentityConflict => "package_identity_conflict",
            Self::Referenced => "package_referenced",
            Self::RootInvalid => "package_root_invalid",
            Self::RootIntact => "package_root_intact",
            Self::Busy => "package_registry_busy",
            Self::RegistryFailed => "package_registry_failed",
            Self::LimitReached => "package_limit_reached",
            Self::ReloadFailed => "package_reload_failed",
            Self::ProfileNotFound => "package_profile_not_found",
            Self::ProfileUnusable => "package_profile_unusable",
            Self::ProfileBaseBuiltin => "package_profile_base_builtin",
            Self::ProfileTargetInvalid => "package_profile_target_invalid",
            Self::ProfileChanged => "package_profile_changed",
        }
    }

    const fn message(self) -> &'static str {
        match self {
            Self::LocalOnly => {
                "runtime package methods are served on the local control socket only"
            }
            Self::ArchiveInvalid => "the package archive is invalid or does not match its digest",
            Self::SourceUnreadable => {
                "the package source or catalog is not an absolute readable path"
            }
            Self::Untrusted => "the package is not authorized by the supplied trust",
            Self::TrustUnavailable => "this host has no catalog trust anchor for official packages",
            Self::Incompatible => "the package does not support this core version or platform",
            Self::DescriptorInvalid => "the package runtime descriptor is not valid",
            Self::RuntimeNotClaimable => "the package may not serve the runtime id it claims",
            Self::RuntimeConflict => "another package or a built-in runtime serves the runtime id",
            Self::NotInstalled => "the package is not installed",
            Self::IdentityInstalled => {
                "the package id and version are already installed from another archive"
            }
            Self::IdentityConflict => "the digest is installed under another package identity",
            Self::Referenced => "a session or host profile still pins the package digest",
            Self::RootInvalid => "the installed package root fails verification",
            Self::RootIntact => {
                "the installed package root verifies and is not removed as modified"
            }
            Self::Busy => "the package registry is locked by another writer",
            Self::RegistryFailed => "the package registry or its storage failed",
            Self::LimitReached => "the package registry holds the maximum number of packages",
            Self::ReloadFailed => {
                "the change was committed but the runtime registry was not rebuilt"
            }
            Self::ProfileNotFound => "the host has no agent profile of that name",
            Self::ProfileUnusable => {
                "the agent profile cannot be rewritten: it fails the profile acceptance rule or is not valid TOML"
            }
            Self::ProfileBaseBuiltin => {
                "the profile's base runtime is served by a built-in, so there is no package to pin"
            }
            Self::ProfileTargetInvalid => {
                "the package cannot be pinned by the profile: it is not installed, is faulted, does not serve the base runtime, or is not the unique selected package"
            }
            Self::ProfileChanged => {
                "the agent profile changed while it was being rewritten and was left as found"
            }
        }
    }

    const fn recover(self) -> &'static str {
        match self {
            Self::LocalOnly => "run the command on the host that runs the daemon",
            Self::ArchiveInvalid => "rebuild the archive and pass its digest again",
            Self::SourceUnreadable => "pass an absolute path the daemon user can read",
            Self::Untrusted => "check the digest or catalog, or install with an explicit digest",
            Self::TrustUnavailable => "install with an explicit digest instead",
            Self::Incompatible => "install a package built for this release",
            Self::DescriptorInvalid => "fix the package's runtime.toml and rebuild it",
            Self::RuntimeNotClaimable => {
                "only a catalog-authorized package may serve codex, claude or hermes, and shell is never claimable"
            }
            Self::RuntimeConflict => "disable or uninstall the package that serves the runtime id",
            Self::NotInstalled => "list installed packages and use an installed digest",
            Self::IdentityInstalled => "bump the package version or uninstall the old archive",
            Self::IdentityConflict => "use the digest of the installed package",
            Self::Referenced => "stop or remove the sessions and profiles that pin the digest",
            Self::RootInvalid => "reinstall the package, or remove it as modified",
            Self::RootIntact => "uninstall the package without the modified-root option",
            Self::Busy => "retry in a moment",
            Self::RegistryFailed => "run the package doctor and check the state directory",
            Self::LimitReached => "uninstall packages that are no longer needed",
            Self::ReloadFailed => "run the package doctor, then retry or restart the daemon",
            Self::ProfileNotFound => "list the profiles with `pohunek plugin profile list`",
            Self::ProfileUnusable => {
                "make the profile a regular owner-private TOML file with a valid base, then retry"
            }
            Self::ProfileBaseBuiltin => "a profile on a built-in base needs no package pin",
            Self::ProfileTargetInvalid => {
                "install, enable and select a package that serves the base runtime, or pass the digest of one"
            }
            Self::ProfileChanged => "run the command again to review the profile as it is now",
        }
    }

    /// The broad error class.
    #[must_use]
    pub const fn class(self) -> ErrorClass {
        match self {
            Self::LocalOnly | Self::Busy | Self::ReloadFailed => ErrorClass::Daemon,
            Self::TrustUnavailable => ErrorClass::Configuration,
            _ => ErrorClass::Runtime,
        }
    }
}

impl From<PackageErrorKind> for ProtocolError {
    fn from(kind: PackageErrorKind) -> Self {
        Self::new(
            kind.class(),
            kind.code(),
            kind.message(),
            Some(kind.recover().to_owned()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest() -> PackageDigest {
        PackageDigest::parse(&format!("sha256:{}", "a".repeat(64))).expect("valid digest")
    }

    #[test]
    fn trust_is_tagged_and_rejects_unknown_fields() {
        let trust = PackageTrust::ExplicitDigest { digest: digest() };
        let value = serde_json::to_value(&trust).expect("serialize");
        assert_eq!(value["kind"], "explicit_digest");
        assert_eq!(
            serde_json::from_value::<PackageTrust>(value).expect("round trip"),
            trust
        );
        let unknown = serde_json::json!({
            "kind": "catalog",
            "catalog_path": "/c.json",
            "extra": true
        });
        serde_json::from_value::<PackageTrust>(unknown).expect_err("rejected");
    }

    #[test]
    fn install_params_reject_unknown_fields_and_missing_flags() {
        let ok = serde_json::json!({
            "archive_path": "/p.tar.zst",
            "trust": { "kind": "explicit_digest", "digest": digest() },
            "enable": true,
            "select": true,
            "dry_run": false
        });
        serde_json::from_value::<PackageInstallParams>(ok.clone()).expect("complete params");
        let mut extra = ok.clone();
        extra["argv"] = serde_json::json!(["sh"]);
        serde_json::from_value::<PackageInstallParams>(extra).expect_err("rejected");
        let mut missing = ok;
        missing.as_object_mut().expect("object").remove("dry_run");
        serde_json::from_value::<PackageInstallParams>(missing).expect_err("rejected");
    }

    #[test]
    fn bind_profile_params_reject_unknown_fields_and_missing_flags() {
        let ok = serde_json::json!({ "profile": "work", "dry_run": true });
        let parsed =
            serde_json::from_value::<PackageBindProfileParams>(ok.clone()).expect("no digest");
        assert_eq!(parsed.digest, None);
        let mut with_digest = ok.clone();
        with_digest["digest"] = serde_json::json!(digest());
        let parsed = serde_json::from_value::<PackageBindProfileParams>(with_digest.clone())
            .expect("with digest");
        assert_eq!(parsed.digest, Some(digest()));
        assert_eq!(
            serde_json::to_value(&parsed).expect("serialize"),
            with_digest
        );
        let mut extra = ok.clone();
        extra["path"] = serde_json::json!("/x");
        serde_json::from_value::<PackageBindProfileParams>(extra).expect_err("rejected");
        let mut missing = ok;
        missing.as_object_mut().expect("object").remove("dry_run");
        serde_json::from_value::<PackageBindProfileParams>(missing).expect_err("rejected");
        assert_eq!(
            serde_json::to_value(PackageBindStatus::Unchanged).expect("serialize"),
            "unchanged"
        );
    }

    #[test]
    fn error_codes_are_unique_and_convert_with_recovery() {
        use std::collections::BTreeSet;
        let kinds = [
            PackageErrorKind::LocalOnly,
            PackageErrorKind::ArchiveInvalid,
            PackageErrorKind::SourceUnreadable,
            PackageErrorKind::Untrusted,
            PackageErrorKind::TrustUnavailable,
            PackageErrorKind::Incompatible,
            PackageErrorKind::DescriptorInvalid,
            PackageErrorKind::RuntimeNotClaimable,
            PackageErrorKind::RuntimeConflict,
            PackageErrorKind::NotInstalled,
            PackageErrorKind::IdentityInstalled,
            PackageErrorKind::IdentityConflict,
            PackageErrorKind::Referenced,
            PackageErrorKind::RootInvalid,
            PackageErrorKind::RootIntact,
            PackageErrorKind::Busy,
            PackageErrorKind::RegistryFailed,
            PackageErrorKind::LimitReached,
            PackageErrorKind::ReloadFailed,
            PackageErrorKind::ProfileNotFound,
            PackageErrorKind::ProfileUnusable,
            PackageErrorKind::ProfileBaseBuiltin,
            PackageErrorKind::ProfileTargetInvalid,
            PackageErrorKind::ProfileChanged,
        ];
        let codes: BTreeSet<_> = kinds.iter().map(|kind| kind.code()).collect();
        assert_eq!(codes.len(), kinds.len());
        for kind in kinds {
            let error = ProtocolError::from(kind);
            assert_eq!(error.code, kind.code());
            assert!(error.recover.is_some());
            assert!(!error.msg.contains('/'), "messages carry no paths");
        }
        assert_eq!(
            ProtocolError::from(PackageErrorKind::LocalOnly).class,
            ErrorClass::Daemon
        );
    }

    #[test]
    fn faults_and_origins_use_snake_case_wire_names() {
        assert_eq!(
            serde_json::to_value(PackageFault::RuntimeNotClaimable).expect("serialize"),
            "runtime_not_claimable"
        );
        assert_eq!(
            serde_json::to_value(PackageOrigin::ExplicitDigest).expect("serialize"),
            "explicit_digest"
        );
        assert_eq!(
            serde_json::to_value(PackageFindingKind::PinnedNotInstalled).expect("serialize"),
            "pinned_not_installed"
        );
    }
}
