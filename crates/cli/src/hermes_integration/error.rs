// Rust guideline compliant 2026-10-13

use std::io::ErrorKind;
use std::sync::LazyLock;

use protocol::{official_package_install_only_step, RuntimeId};

/// Recovery for [`Error::PackageNotInstalled`]: the official package install
/// step, plus the one action that needs no package. A disabled installed
/// package is `PackageNotSelected`, so the hint offers no enable step. Built
/// once because the install step is formatted.
static PACKAGE_NOT_INSTALLED_HINT: LazyLock<String> = LazyLock::new(|| {
    format!(
        "{}, then retry; `pohunek integration uninstall --agent hermes` with the original `--hermes-profile` or `--hermes-home` works without the package",
        official_package_install_only_step(&RuntimeId::hermes())
    )
});

/// A redacted failure while validating Hermes integration inputs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Error {
    /// The supplied target selection was not safe to resolve.
    #[error("Hermes target is unsafe")]
    UnsafeTarget,
    /// A path that must be absolute was relative.
    #[error("an absolute path is required")]
    RelativePath,
    /// The selected profile name was not supported by Hermes semantics.
    #[error("Hermes profile name is invalid")]
    InvalidProfile,
    /// The selected profile name is reserved.
    #[error("Hermes profile name is reserved")]
    ReservedProfile,
    /// An existing filesystem entry had an unsafe type or ownership.
    #[error("Hermes target ownership or permissions are unsafe")]
    UnsafePermissions,
    /// An I/O operation needed for validation failed.
    #[error("Hermes target validation failed ({kind:?})")]
    Io {
        /// The redacted I/O error kind.
        kind: ErrorKind,
    },
    /// The requested policy schema revision is not supported.
    #[error("unsupported Hermes policy schema")]
    UnsupportedPolicySchema,
    /// A policy field was missing, malformed, or outside its safe bound.
    #[error("Hermes policy is invalid")]
    InvalidPolicy,
    /// The stored Pohunek executable did not meet the fixed runner contract.
    #[error("Pohunek CLI path is not a canonical executable")]
    InvalidCliPath,
    /// A wildcard host was selected without caller confirmation.
    #[error("wildcard host requires explicit confirmation")]
    WildcardConfirmationRequired,
    /// The derived policy location would be inside the managed plugin tree.
    #[error("Hermes policy location is unsafe")]
    UnsafePolicyPath,
    /// The selected operator source failed integrity or the closed sidecar contract.
    #[error("Hermes plugin asset manifest is invalid")]
    InvalidAsset,
    /// No official Hermes runtime package is installed, so no operator source exists.
    #[error("no Hermes runtime package is installed")]
    PackageNotInstalled,
    /// The runtime package registry could not be opened or read safely.
    #[error("the runtime package registry is unreadable")]
    PackageRegistryUnreadable,
    /// A Hermes package is installed, but no enabled official version is selected.
    #[error("no enabled official Hermes package is selected")]
    PackageNotSelected,
    /// A directory exists but was not created by this lifecycle.
    #[error("Hermes plugin directory is not owned by Pohunek")]
    Collision,
    /// The ownership marker was absent, malformed, or inconsistent.
    #[error("Hermes plugin ownership marker is invalid")]
    InvalidMarker,
    /// The controlled Hermes subprocess did not satisfy its fixed contract.
    #[error("Hermes command failed")]
    HermesCommand,
    /// The selected Hermes executable was not a private, canonical executable.
    #[error("Hermes executable is not a canonical safe executable")]
    InvalidHermesExecutable,
    /// The pinned sibling Python runtime was absent or unsafe.
    #[error("Hermes Python runtime is not a canonical safe executable")]
    InvalidHermesRuntime,
    /// The controlled Hermes subprocess exceeded its bounded execution time.
    #[error("Hermes command timed out")]
    HermesTimeout,
    /// The controlled Hermes subprocess exceeded its bounded output contract.
    #[error("Hermes command exceeded its output limit")]
    HermesOutputLimit,
    /// Hermes returned an unsupported fixed plugin lifecycle state.
    #[error("Hermes plugin state is invalid")]
    InvalidHermesState,
    /// The installed Hermes release lies outside the range the selected Hermes
    /// package declares, or its version output does not parse.
    #[error("Hermes version is unsupported")]
    UnsupportedHermes,
    /// A caller must acknowledge a destructive managed-file removal.
    #[error("modified managed files require explicit confirmation")]
    ConfirmationRequired,
    /// A staged plugin failed fixed syntax or schema validation.
    #[error("staged Hermes plugin validation failed")]
    StagedValidation,
    /// The installed plugin failed its bounded registration and hook probe.
    #[error("installed Hermes plugin probe failed")]
    InstalledProbe,
    /// A filesystem transaction could not be restored safely.
    #[error("Hermes lifecycle recovery is required")]
    RecoveryRequired,
    /// Another process currently owns the Hermes lifecycle transaction lock.
    #[error("another Hermes lifecycle transaction is already running")]
    TransactionBusy,
}

impl Error {
    /// Returns a payload-free recovery hint for this failure.
    #[must_use]
    pub(crate) fn recovery_hint(&self) -> &'static str {
        match self {
            Self::UnsafeTarget | Self::RelativePath => {
                "select an explicit, private Hermes profile or custom home"
            }
            Self::InvalidProfile | Self::ReservedProfile => {
                "use `default` or a lowercase Hermes profile name"
            }
            Self::UnsafePermissions => "repair ownership and remove group/world write permission",
            Self::Io { .. } => "check the selected path without exposing it in diagnostics",
            Self::UnsupportedPolicySchema => "use a policy written by this Pohunek version",
            Self::InvalidPolicy => {
                "provide every required policy field within its documented bound"
            }
            Self::InvalidCliPath => "select an existing absolute Pohunek executable",
            Self::WildcardConfirmationRequired => {
                "confirm the wildcard host explicitly before creating the policy"
            }
            Self::UnsafePolicyPath => "select a Pohunek configuration root outside the plugin tree",
            Self::InvalidAsset => {
                "verify the selected Hermes package with `pohunek plugin doctor` and reinstall it from its signed catalog"
            }
            Self::PackageNotInstalled => PACKAGE_NOT_INSTALLED_HINT.as_str(),
            Self::PackageRegistryUnreadable => {
                "run `pohunek plugin doctor` and repair the owner-private runtime package registry before retrying"
            }
            Self::PackageNotSelected => {
                "enable and select an official `pohunek.runtime.hermes` version with `pohunek plugin enable` and `pohunek plugin select`, or install one from its signed catalog"
            }
            Self::Collision => "choose a profile without an unrelated `pohunek` plugin",
            Self::InvalidMarker => "repair or manually preserve the affected plugin directory",
            Self::HermesCommand => "inspect the Hermes command result without exposing its output",
            Self::InvalidHermesExecutable => {
                "select the pinned Hermes executable owned by the current user"
            }
            Self::InvalidHermesRuntime => {
                "repair the pinned sibling Python runtime before retrying"
            }
            Self::HermesTimeout => "retry after Hermes is responsive",
            Self::HermesOutputLimit => "repair the local Hermes installation before retrying",
            Self::InvalidHermesState => {
                "repair the fixed Pohunek plugin registration before retrying"
            }
            Self::UnsupportedHermes => {
                "use a Hermes release the selected Hermes package supports"
            }
            Self::ConfirmationRequired => {
                "repeat the action with explicit modification confirmation"
            }
            Self::StagedValidation => {
                "use a supported local Python runtime and a valid operator asset source"
            }
            Self::InstalledProbe => {
                "repair the managed plugin, policy, or Pohunek CLI before retrying"
            }
            Self::RecoveryRequired => {
                "preserve the reported managed recovery directory and run Hermes doctor"
            }
            Self::TransactionBusy => "wait for the active Hermes lifecycle action and retry",
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io { kind: error.kind() }
    }
}
