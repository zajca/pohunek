// Rust guideline compliant 2026-08-06

use std::io::ErrorKind;

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
    /// An embedded plugin asset did not meet the fixed ownership contract.
    #[error("Hermes plugin asset manifest is invalid")]
    InvalidAsset,
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
    /// The installed Hermes version is not supported by this lifecycle.
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
}

impl Error {
    /// Returns a payload-free recovery hint for this failure.
    #[must_use]
    pub(crate) const fn recovery_hint(&self) -> &'static str {
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
            Self::InvalidAsset => "use the plugin assets embedded by this Pohunek release",
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
            Self::UnsupportedHermes => "use the pinned supported Hermes version",
            Self::ConfirmationRequired => {
                "repeat the action with explicit modification confirmation"
            }
            Self::StagedValidation => "use a supported local Python runtime and embedded assets",
            Self::InstalledProbe => {
                "repair the managed plugin, policy, or Pohunek CLI before retrying"
            }
            Self::RecoveryRequired => {
                "preserve the reported managed recovery directory and run Hermes doctor"
            }
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io { kind: error.kind() }
    }
}
