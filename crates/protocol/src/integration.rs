//! Typed payloads for hook integration installation.
//!
//! The `integration.install` control method asks the daemon to install the
//! per-agent `SessionStart` hook that captures each agent's native session id
//! for resume (see `docs/plan-phase-1.md` "Hook Integration"). The daemon owns
//! the install because it runs as the same user and writes into the agent's
//! config dir (`~/.claude`, `~/.codex`).

use serde::{Deserialize, Serialize};

use crate::RuntimeRef;

/// Gate flag the daemon sets so the agent hook knows it was launched by pohunek.
///
/// These `ENV_*` names are the daemon↔agent↔hook handshake contract, so they
/// live in `protocol` (the shared contract crate) as the single source of truth:
/// the daemon injects them into each agent PTY, the installed hook reads them to
/// call home, and the CLI reads [`ENV_SESSION_ID`] to detect a self-feeding
/// attach (a `pohunek attach` launched from inside the very session it targets).
pub const ENV_FLAG: &str = "POHUNEK_ENV";
/// Control-socket path the hook dials to report the native session id.
pub const ENV_SOCKET_PATH: &str = "POHUNEK_SOCKET_PATH";
/// The pohunek session id the agent was launched under. Present in every process
/// running inside a session's PTY, so a CLI invoked there can tell the daemon
/// which session it originates from (self-feeding-attach guard).
pub const ENV_SESSION_ID: &str = "POHUNEK_SESSION_ID";
/// Opaque per-daemon-instance id, injected into every session PTY alongside
/// [`ENV_SESSION_ID`]. The CLI echoes it back as the attach origin so the daemon
/// can pin a self-feeding attach to **its own** running instance — distinguishing
/// "attaching to the session I am inside" (same id AND same instance → reject)
/// from a different daemon that merely reuses the same session-id string, and
/// from a stale value left by a previous daemon process (different instance →
/// allow). Regenerated on every daemon start; never persisted.
pub const ENV_DAEMON_ID: &str = "POHUNEK_DAEMON_ID";
/// Stable worker identity inherited by every process in a managed PTY.
pub const ENV_WORKER_ID: &str = "POHUNEK_WORKER_ID";
/// Owner-private worker endpoint used by durable identity hooks.
pub const ENV_WORKER_SOCKET_PATH: &str = "POHUNEK_WORKER_SOCKET_PATH";
/// Private worker-hook protocol version injected at runtime.
pub const ENV_WORKER_PROTOCOL_VERSION: &str = "POHUNEK_WORKER_PROTOCOL_VERSION";
/// Wire protocol version the hook must stamp on its request envelope.
///
/// Injected (rather than baked into the asset) so the hook never hardcodes the
/// protocol version: [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION) is the single
/// source of truth.
pub const ENV_PROTOCOL_VERSION: &str = "POHUNEK_PROTOCOL_VERSION";

/// Expected integration asset version reported by `integration.status`.
///
/// Every managed daemon hook asset carries this version marker. It is exposed
/// through `protocol` because CLI and SDK consumers need the same expected value
/// without linking the daemon implementation.
pub const EXPECTED_INTEGRATION_VERSION: u32 = 11;

/// Parameters for `integration.install`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "IntegrationInstallParams.ts"))]
pub struct IntegrationInstallParams {
    /// Agent to install the hook for. When omitted, the daemon installs the
    /// hook for every supported agent whose config dir is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub agent: Option<RuntimeRef>,
}

/// Result returned by `integration.install`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "IntegrationInstallResult.ts"))]
pub struct IntegrationInstallResult {
    /// One report per agent the hook was installed for.
    pub installed: Vec<IntegrationInstallReport>,
}

/// Request parameters for `integration.status`.
///
/// Unknown fields are rejected so misspelled filters cannot broaden a report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "IntegrationStatusParams.ts"))]
pub struct IntegrationStatusParams {
    /// Restrict the read-only report to one agent. When omitted, report every
    /// supported hook agent regardless of whether its config dir exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub agent: Option<RuntimeRef>,
}

/// Result returned by `integration.status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "IntegrationStatusResult.ts"))]
pub struct IntegrationStatusResult {
    /// One read-only report per requested (or supported) hook agent.
    pub agents: Vec<IntegrationAgentStatus>,
}

/// Read-only installation state for one managed hook integration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "IntegrationAgentStatus.ts"))]
pub struct IntegrationAgentStatus {
    /// Agent the report describes.
    pub agent: RuntimeRef,
    /// Whether the agent's configuration directory exists.
    pub available: bool,
    /// Expected managed asset paths, including files that are absent.
    pub expected_asset_paths: Vec<String>,
    /// Managed assets present on disk. Paths never contain secret values.
    pub present_asset_paths: Vec<String>,
    /// Registration files inspected for this agent.
    pub registration_paths: Vec<String>,
    /// Common version marker found across readable managed assets.
    pub installed_version: Option<u32>,
    /// Version currently embedded in this build.
    pub expected_version: u32,
    /// Aggregate install health derived from the files above.
    pub state: IntegrationInstallState,
    /// Safe next action derived from the complete set of findings.
    pub recovery: IntegrationRecovery,
    /// Non-fatal, non-secret reasons the complete install contract is unhealthy.
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// Safe recovery action for one managed hook integration report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "IntegrationRecovery.ts"))]
pub enum IntegrationRecovery {
    /// The complete managed integration contract is current.
    None,
    /// Re-running the installer can repair every reported finding.
    Reinstall,
    /// Provider configuration must be repaired before reinstalling safely.
    RepairConfiguration,
}

/// Derived installation health for one managed hook integration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "IntegrationInstallState.ts"))]
pub enum IntegrationInstallState {
    /// No Pohunek-managed asset or registration was detected.
    NotInstalled,
    /// Every managed asset, registration, and trust record matches the installer.
    Current,
    /// A detected or unreadable install is incomplete, modified, or malformed.
    Outdated,
}

/// Per-agent record of what the installer wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "IntegrationInstallReport.ts"))]
pub struct IntegrationInstallReport {
    /// Agent the hook was installed for.
    pub agent: RuntimeRef,
    /// Absolute path of the installed hook script.
    pub hook_path: String,
    /// Config files the installer created or merged into (settings.json /
    /// hooks.json / config.toml), in the order they were touched.
    pub config_paths: Vec<String>,
    /// Quarantined originals whose deletion did not finish after the install
    /// committed, each with its quarantine path and why. The install itself
    /// succeeded; empty when cleanup completed.
    #[serde(default)]
    pub cleanup_incomplete: Vec<String>,
}

/// Parameters for `integration.uninstall`.
///
/// Removal is destructive, so the agent is always named: a request without one
/// is rejected instead of widening to every agent, and unknown fields are
/// rejected so a misspelled selector cannot widen a removal either.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "IntegrationUninstallParams.ts")
)]
pub struct IntegrationUninstallParams {
    /// Agent to remove the managed hooks for.
    pub agent: RuntimeRef,
}

/// Outcome of removing one agent's managed hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "IntegrationUninstallState.ts"))]
pub enum IntegrationUninstallState {
    /// At least one managed asset or registration was removed.
    Removed,
    /// Nothing owned by the installer was present, so nothing changed.
    NotInstalled,
}

/// Per-agent record of what the uninstaller changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "IntegrationUninstallReport.ts")
)]
pub struct IntegrationUninstallReport {
    /// Agent whose managed hooks were removed.
    pub agent: RuntimeRef,
    /// Whether anything was removed.
    pub state: IntegrationUninstallState,
    /// Managed hook scripts that were deleted.
    pub removed_paths: Vec<String>,
    /// Provider registration files edited to drop only managed entries.
    pub updated_paths: Vec<String>,
    /// Entries at managed script paths that the installer does not own (a
    /// symlink, a non-regular file, or a file without the ownership marker)
    /// and that were therefore left untouched.
    pub preserved_paths: Vec<String>,
    /// Quarantined originals whose deletion did not finish after the removal
    /// committed, each with its quarantine path and why. The removal itself
    /// succeeded; empty when cleanup completed.
    #[serde(default)]
    pub cleanup_incomplete: Vec<String>,
}

/// Result returned by `integration.uninstall`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "IntegrationUninstallResult.ts")
)]
pub struct IntegrationUninstallResult {
    /// The report for the agent the removal ran for.
    pub uninstalled: Vec<IntegrationUninstallReport>,
}

/// Request parameters for `integration.doctor`.
///
/// Unknown fields are rejected so misspelled filters cannot broaden a report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "IntegrationDoctorParams.ts"))]
pub struct IntegrationDoctorParams {
    /// Restrict the read-only diagnosis to one agent. When omitted, diagnose
    /// every supported hook agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub agent: Option<RuntimeRef>,
}

/// Stable identifier of one doctor finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "IntegrationFindingCode.ts"))]
pub enum IntegrationFindingCode {
    /// The optional agent's config directory does not exist on this host.
    AgentNotInstalled,
    /// The agent is present but Pohunek's managed hooks are not installed.
    HooksNotInstalled,
    /// The agent config path is unresolvable, not a directory, or unreadable.
    ConfigRootInvalid,
    /// A managed hook script is absent.
    AssetMissing,
    /// A managed hook script differs from the embedded asset or is unreadable.
    AssetModified,
    /// A managed script or its parent has an unsafe owner, mode, or type.
    AssetUnsafe,
    /// A registration entry or file is missing, duplicated, or modified.
    RegistrationDrift,
    /// A provider configuration file is malformed or has an unusable shape.
    ProviderConfigInvalid,
    /// Codex hooks are not enabled in `config.toml`.
    CodexHooksFeatureDisabled,
    /// A Codex managed trust record is missing, modified, or stale.
    CodexTrustDrift,
    /// Informational: the first `python3` on the daemon's `PATH` and where it is.
    HookRuntimePythonFound,
    /// Informational: no `python3` was found on the daemon's `PATH`. The hooks
    /// run `python3` from the agent's own `PATH`, which the daemon cannot see.
    HookRuntimeMissing,
    /// Informational: the first `python3` on the daemon's `PATH` is the macOS
    /// Command Line Tools stub and no developer tools back it.
    HookRuntimeMacosShim,
    /// The daemon's own runtime socket path cannot be bound or reached.
    HookSocketPathInvalid,
    /// The installer lock file is not a regular owner-private file, so every
    /// install and uninstall fails until it is repaired.
    UnsafeInstallerLock,
    /// A quarantined original from an earlier install or removal was left in an
    /// agent config directory.
    DisplacedOriginalLeftBehind,
    /// An install or uninstall currently holds the installer lock, so nothing
    /// was scanned or concluded for this agent; run the doctor again later.
    OperationInProgress,
    /// The quarantine scan could not cover every directory entry, so leftover
    /// originals may exist that were not reported.
    QuarantineScanIncomplete,
    /// Drift that no more specific finding describes.
    InstallDrift,
}

/// Severity of one doctor finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts",
    ts(export, export_to = "IntegrationFindingSeverity.ts")
)]
pub enum IntegrationFindingSeverity {
    /// Informational; never makes the diagnosis fail or changes its exit code.
    Info,
    /// The integration cannot work until the finding is remediated.
    Error,
}

/// One diagnosed cause with its remediation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "IntegrationFinding.ts"))]
pub struct IntegrationFinding {
    /// Stable cause identifier.
    pub code: IntegrationFindingCode,
    /// Whether the finding fails the diagnosis.
    pub severity: IntegrationFindingSeverity,
    /// Non-secret description of what was observed.
    pub summary: String,
    /// Operator action that resolves the finding, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub remediation: Option<String>,
}

/// Read-only diagnosis of one managed hook integration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "IntegrationAgentDoctor.ts"))]
pub struct IntegrationAgentDoctor {
    /// Agent the diagnosis describes.
    pub agent: RuntimeRef,
    /// Whether the diagnosis has no error finding.
    pub ok: bool,
    /// The read-only status the findings were derived from; absent when an
    /// install or uninstall held the installer lock and nothing was read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub status: Option<IntegrationAgentStatus>,
    /// Every observed cause, in a stable order.
    pub findings: Vec<IntegrationFinding>,
}

/// Result returned by `integration.doctor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "IntegrationDoctorResult.ts"))]
pub struct IntegrationDoctorResult {
    /// Whether every diagnosed agent is ok.
    pub ok: bool,
    /// One diagnosis per requested (or supported) hook agent.
    pub agents: Vec<IntegrationAgentDoctor>,
}

#[cfg(test)]
mod tests {
    use super::{IntegrationInstallState, IntegrationRecovery, IntegrationStatusParams};

    #[test]
    fn integration_status_params_default_to_all_agents() {
        assert_eq!(IntegrationStatusParams::default().agent, None);
    }

    #[test]
    fn integration_status_params_reject_unknown_fields() {
        let error = serde_json::from_value::<IntegrationStatusParams>(serde_json::json!({
            "agent": "codex",
            "unexpected": true,
        }))
        .expect_err("unknown integration status fields must fail");

        assert!(error.to_string().contains("unknown field `unexpected`"));
    }

    #[test]
    fn integration_install_states_use_exact_snake_case_wire_values() {
        for (state, expected) in [
            (IntegrationInstallState::NotInstalled, "\"not_installed\""),
            (IntegrationInstallState::Current, "\"current\""),
            (IntegrationInstallState::Outdated, "\"outdated\""),
        ] {
            assert_eq!(
                serde_json::to_string(&state).expect("serialize integration state"),
                expected
            );
        }
    }

    #[test]
    fn integration_recovery_uses_exact_snake_case_wire_values() {
        for (recovery, expected) in [
            (IntegrationRecovery::None, "\"none\""),
            (IntegrationRecovery::Reinstall, "\"reinstall\""),
            (
                IntegrationRecovery::RepairConfiguration,
                "\"repair_configuration\"",
            ),
        ] {
            assert_eq!(
                serde_json::to_string(&recovery).expect("serialize integration recovery"),
                expected
            );
        }
    }

    #[test]
    fn integration_uninstall_and_doctor_params_reject_unknown_fields() {
        let uninstall = serde_json::from_value::<super::IntegrationUninstallParams>(
            serde_json::json!({ "agent": "claude", "everything": true }),
        )
        .expect_err("unknown uninstall fields must fail");
        assert!(uninstall.to_string().contains("unknown field `everything`"));
        let doctor = serde_json::from_value::<super::IntegrationDoctorParams>(
            serde_json::json!({ "agent": "codex", "deep": true }),
        )
        .expect_err("unknown doctor fields must fail");
        assert!(doctor.to_string().contains("unknown field `deep`"));
        assert_eq!(super::IntegrationDoctorParams::default().agent, None);
        for missing in [serde_json::json!({}), serde_json::Value::Null] {
            assert!(
                serde_json::from_value::<super::IntegrationUninstallParams>(missing).is_err(),
                "an uninstall must name its agent"
            );
        }
    }

    #[test]
    fn integration_finding_enums_use_exact_snake_case_wire_values() {
        use super::{IntegrationFindingCode as Code, IntegrationFindingSeverity as Severity};
        for (code, expected) in [
            (Code::AgentNotInstalled, "\"agent_not_installed\""),
            (Code::HooksNotInstalled, "\"hooks_not_installed\""),
            (Code::ConfigRootInvalid, "\"config_root_invalid\""),
            (Code::AssetMissing, "\"asset_missing\""),
            (Code::AssetModified, "\"asset_modified\""),
            (Code::AssetUnsafe, "\"asset_unsafe\""),
            (Code::RegistrationDrift, "\"registration_drift\""),
            (Code::ProviderConfigInvalid, "\"provider_config_invalid\""),
            (
                Code::CodexHooksFeatureDisabled,
                "\"codex_hooks_feature_disabled\"",
            ),
            (Code::CodexTrustDrift, "\"codex_trust_drift\""),
            (
                Code::HookRuntimePythonFound,
                "\"hook_runtime_python_found\"",
            ),
            (Code::HookRuntimeMissing, "\"hook_runtime_missing\""),
            (Code::HookRuntimeMacosShim, "\"hook_runtime_macos_shim\""),
            (Code::HookSocketPathInvalid, "\"hook_socket_path_invalid\""),
            (
                Code::DisplacedOriginalLeftBehind,
                "\"displaced_original_left_behind\"",
            ),
            (Code::UnsafeInstallerLock, "\"unsafe_installer_lock\""),
            (Code::OperationInProgress, "\"operation_in_progress\""),
            (
                Code::QuarantineScanIncomplete,
                "\"quarantine_scan_incomplete\"",
            ),
            (Code::InstallDrift, "\"install_drift\""),
        ] {
            assert_eq!(
                serde_json::to_string(&code).expect("serialize code"),
                expected
            );
        }
        for (severity, expected) in [(Severity::Info, "\"info\""), (Severity::Error, "\"error\"")] {
            assert_eq!(
                serde_json::to_string(&severity).expect("serialize severity"),
                expected
            );
        }
    }

    #[test]
    fn integration_uninstall_state_uses_exact_snake_case_wire_values() {
        for (state, expected) in [
            (super::IntegrationUninstallState::Removed, "\"removed\""),
            (
                super::IntegrationUninstallState::NotInstalled,
                "\"not_installed\"",
            ),
        ] {
            assert_eq!(serde_json::to_string(&state).expect("serialize"), expected);
        }
    }
}
