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
pub const ENSURE_REPORT_VERSION: u32 = 2;

/// Argument of `pohunekd` that runs the step.
pub const ENSURE_RUNTIMES_ARG: &str = "ensure-official-runtimes";

/// Flag after [`ENSURE_RUNTIMES_ARG`] that computes and prints the plan
/// without changing anything. A dry run reads only, so it may run while the
/// previous release's daemon still runs.
pub const DRY_RUN_FLAG: &str = "--dry-run";

/// Flag after [`DRY_RUN_FLAG`] that also plans every runtime of the release
/// catalog whose program is on the search path, and reports what cannot be
/// planned as a [`Gap`]. It is a plan only: without [`DRY_RUN_FLAG`] it is a
/// usage error.
pub const DETECT_FLAG: &str = "--detect";

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

/// A host profile is pinned to a package version that is newer than the
/// release package. The step keeps the pin, so a downgrade is never applied
/// to it.
pub const CODE_PINNED_TO_NEWER_VERSION: &str = "pinned_to_newer_version";
/// A host profile is pinned to a different package of the same version as the
/// release package. Such a pin is anomalous, so the step keeps it.
pub const CODE_PINNED_TO_SAME_VERSION: &str = "pinned_to_same_version";
/// A host profile is pinned to a package that is not an official one, such as
/// a local package installed by explicit digest. The step never repins it.
pub const CODE_PINNED_TO_NON_OFFICIAL_PACKAGE: &str = "pinned_to_non_official_package";
/// A host profile is pinned to a package with another package id than the
/// release package of its runtime. The step never repins it.
pub const CODE_PINNED_TO_OTHER_PACKAGE: &str = "pinned_to_other_package";
/// A host profile is pinned to a digest the registry has no record of, or to a
/// version that cannot be ordered against the release package. The step keeps
/// the pin.
pub const CODE_PINNED_VERSION_UNKNOWN: &str = "pinned_version_unknown";
/// The release package of a host profile's runtime is recorded as a package
/// that is not official, so a repin cannot target it. The step keeps the pin.
pub const CODE_PINNED_TARGET_NOT_OFFICIAL: &str = "pinned_target_not_official";

/// A runtime's program is not on the search path, so its agent is not
/// installed for this host.
pub const CODE_GAP_AGENT_NOT_ON_PATH: &str = "agent_not_on_search_path";
/// A runtime's declared config home does not exist. The package is still
/// planned; the home is never created by the step.
pub const CODE_GAP_CONFIG_HOME_MISSING: &str = "config_home_missing";
/// A runtime's declared config home is a symlink. The installer writes hooks
/// only into a real directory, so no integration is planned for it.
pub const CODE_GAP_CONFIG_HOME_SYMLINK: &str = "config_home_is_symlink";
/// Another package already serves the runtime id. It is left untouched.
pub const CODE_GAP_SERVED_BY_OTHER_PACKAGE: &str = "runtime_served_by_other_package";

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
    /// One entry per host profile over an official runtime the step binds,
    /// repins or keeps. A profile already pinned to the release package has no
    /// entry.
    pub profiles: Vec<ProfileAction>,
    /// The config homes whose daemon-installed integration hooks the
    /// installer reinstalls once the new daemon is ready.
    ///
    /// Without `--detect`, one entry per home that already holds hooks. With
    /// `--detect`, one entry per existing home of a planned runtime whose
    /// hooks the setup installs, whether or not hooks are present yet. Each
    /// distinct home appears once.
    pub integrations: Vec<IntegrationAction>,
    /// Why the plan cannot be applied; empty when it can.
    pub refusals: Vec<Refusal>,
    /// What `--detect` found that the setup must resolve or accept, one next
    /// step each. Always empty without `--detect`, and a gap never blocks an
    /// apply that has no refusal.
    #[serde(default)]
    pub gaps: Vec<Gap>,
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
    /// Its program is on the search path, found by `--detect`.
    Detected,
}

/// A condition `--detect` reports without refusing the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gap {
    /// The runtime id the gap is about.
    pub runtime: String,
    /// One of the `CODE_*` or `CODE_GAP_*` constants of this module, or a
    /// package error code of the daemon.
    pub code: String,
    /// Operator-facing detail with the one next step.
    pub detail: String,
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
    /// Set when the registry selects a newer official version of the package
    /// than the release package this step installs and selects. The step does
    /// not keep that selection: it selects the release package, which the
    /// operator can select again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces_newer: Option<SelectedPackage>,
}

/// A package the registry selects, named by its digest and version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedPackage {
    /// Archive digest of the selected package.
    pub digest: String,
    /// Package version of the selected package.
    pub version: String,
}

/// The state of one host profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileStatus {
    /// The plan pins a profile that has no pin to the package.
    Bind,
    /// The step pinned the profile to the package.
    Bound,
    /// The plan moves a profile pinned to an older official version of the
    /// package to the release package.
    Repin,
    /// The step moved the profile to the release package.
    Repinned,
    /// The profile keeps its pin, and [`ProfileAction::kept_reason`] says why.
    Kept,
}

impl ProfileStatus {
    /// Whether the plan changes the profile's pin, which the step then applies.
    #[must_use]
    pub fn is_pending(self) -> bool {
        matches!(self, Self::Bind | Self::Repin)
    }

    /// Whether the step has changed the profile's pin.
    #[must_use]
    pub fn is_applied(self) -> bool {
        matches!(self, Self::Bound | Self::Repinned)
    }
}

/// One host profile over an official runtime the step binds, repins or keeps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileAction {
    /// Profile name.
    pub name: String,
    /// The runtime id the profile is based on.
    pub base: String,
    /// The package digest the profile is pinned to after the step: the target
    /// of a bind or repin, or the digest a kept profile stays pinned to.
    pub digest: String,
    /// What the step does or did.
    pub status: ProfileStatus,
    /// The digest a repin moves the profile off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_digest: Option<String>,
    /// The version of the package the profile is pinned to before the step,
    /// when the registry records it. Set for a repin and for a kept profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_version: Option<String>,
    /// The version of the package the profile is pinned to by a bind or
    /// repin. Not set for a kept profile, which keeps its own pin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Why a kept profile is left unchanged: one of the `CODE_PINNED_*`
    /// constants of this module. Set only for a kept profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept_reason: Option<String>,
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
