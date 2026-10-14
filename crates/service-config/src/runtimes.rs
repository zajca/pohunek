//! The report `pohunekd ensure-official-runtimes` prints and `pohunek service
//! upgrade` reads.
//!
//! The new daemon binary installs, from the release's own `runtime/`
//! directory, the official runtime packages the host uses and pins the host
//! profiles over their runtime ids to them. It runs while no daemon runs, so
//! the package and profile changes land before the new daemon starts. The
//! report is the one contract between that binary and the installer, so both
//! link these types. Codes are stable machine strings.

// Rust guideline compliant 2026-10-14

use serde::{Deserialize, Serialize};

/// Version of the report layout; a reader refuses any other value.
pub const ENSURE_REPORT_VERSION: u32 = 1;

/// Argument of `pohunekd` that runs the step.
pub const ENSURE_RUNTIMES_ARG: &str = "ensure-official-runtimes";

/// Flag after [`ENSURE_RUNTIMES_ARG`] that computes and prints the plan
/// without changing anything. A dry run reads only, so it may run while the
/// previous release's daemon still runs.
pub const DRY_RUN_FLAG: &str = "--dry-run";

/// The release directory below the version directory that holds the catalog
/// and the official package archives.
pub const RELEASE_RUNTIME_DIR: &str = "runtime";

/// The release has no usable `runtime/` catalog for this host, and a runtime
/// the host uses has no installed package that serves it.
pub const CODE_RELEASE_CATALOG_UNAVAILABLE: &str = "release_runtime_catalog_unavailable";
/// The release catalog lists no package of a runtime the host uses for this
/// platform.
pub const CODE_PACKAGE_UNAVAILABLE_FOR_PLATFORM: &str = "official_package_unavailable_for_platform";
/// A listed package archive is missing from the release or fails its checks.
pub const CODE_PACKAGE_ARCHIVE_INVALID: &str = "official_package_archive_invalid";
/// A host profile over an official runtime cannot be pinned to its package.
pub const CODE_PROFILE_NOT_MIGRATABLE: &str = "profile_not_migratable";
/// A daemon holds the instance lock, so nothing may change offline.
pub const CODE_DAEMON_RUNNING: &str = "daemon_running";
/// The release runtime plan cannot be computed at all, so the upgrade refuses
/// before any change.
pub const CODE_RUNTIME_PLAN_UNAVAILABLE: &str = "runtime_plan_unavailable";
/// The installed `service.toml` cannot be read, so the environment a launch
/// inherits, and therefore the plan and the version probes, are unknown.
pub const CODE_SERVICE_CONFIG_UNREADABLE: &str = "service_config_unreadable";

/// What the step found, planned and (unless a dry run) did.
///
/// A report with any [`EnsureReport::refusals`] changed nothing: the step
/// applies its plan only when the whole plan is possible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnsureReport {
    /// [`ENSURE_REPORT_VERSION`].
    pub report_version: u32,
    /// Version of the daemon binary that produced the report.
    pub daemon_version: String,
    /// Whether the report is a plan only.
    pub dry_run: bool,
    /// One entry per official runtime the host uses.
    pub packages: Vec<PackageAction>,
    /// Whether the package catalog trust state records the release
    /// catalog's sequence and revoked keys.
    pub catalog: CatalogStatus,
    /// One entry per host profile over an official runtime that pins no
    /// package.
    pub profiles: Vec<ProfileAction>,
    /// The daemon-installed integration hooks the installer reinstalls once
    /// the new daemon is ready.
    pub integrations: Vec<IntegrationAction>,
    /// Why the plan cannot be applied; empty when it can.
    pub refusals: Vec<Refusal>,
}

/// Why the host uses an official runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UseReason {
    /// A host profile is based on it.
    Profile,
    /// A live or stopped session record was launched from it.
    Session,
    /// Its integration hooks are installed.
    Integration,
}

/// The state of one official runtime's package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageStatus {
    /// The release's package serves the runtime already (installed, enabled
    /// and selected); nothing to do.
    Present,
    /// The plan installs, enables and selects the release's package.
    Install,
    /// The step installed, enabled and selected the release's package.
    Installed,
    /// The release has no usable catalog, and an installed package that
    /// loads serves the runtime; it keeps serving it. The action names that
    /// package.
    Kept,
}

/// The package catalog trust state against the release catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogStatus {
    /// The release has no usable catalog, so the trust state is not read.
    Unavailable,
    /// The recorded trust state is at or past the release catalog's sequence
    /// and holds its revoked keys; nothing to do.
    Current,
    /// The plan records the release catalog's trust state.
    Record,
    /// The step recorded the release catalog's trust state.
    Recorded,
}

/// One official runtime the host uses and the release package for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageAction {
    /// Runtime id (`claude`, `codex`, `hermes`).
    pub runtime: String,
    /// Package id from the release catalog.
    pub package: String,
    /// Package version from the release catalog.
    pub version: String,
    /// Archive digest from the release catalog.
    pub digest: String,
    /// What the step does or did.
    pub status: PackageStatus,
    /// Why the host uses the runtime.
    pub reasons: Vec<UseReason>,
}

/// The state of one host profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileStatus {
    /// The plan pins the profile to the package.
    Bind,
    /// The step pinned the profile to the package.
    Bound,
}

/// One host profile over an official runtime that pins no package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileAction {
    /// Profile name.
    pub name: String,
    /// The runtime id the profile is based on.
    pub base: String,
    /// The package digest the profile is pinned to.
    pub digest: String,
    /// What the step does or did.
    pub status: ProfileStatus,
}

/// Integration hooks the installer reinstalls through the new daemon.
///
/// One entry per config home that holds the hooks: the runtime's default home
/// or the home a host profile selects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationAction {
    /// Runtime id whose daemon-installed hooks are present.
    pub runtime: String,
    /// The host profile whose config home holds the hooks; `None` for the
    /// runtime's default home.
    pub profile: Option<String>,
}

/// A reason the plan cannot be applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    /// The runtime id or profile name the refusal is about.
    pub subject: String,
    /// One of the `CODE_*` constants of this module, or a package or profile
    /// error code of the daemon.
    pub code: String,
    /// Operator-facing detail with the next step.
    pub detail: String,
}
