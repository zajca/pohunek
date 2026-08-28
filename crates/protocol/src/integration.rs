//! Typed payloads for hook integration installation.
//!
//! The `integration.install` control method asks the daemon to install the
//! per-agent `SessionStart` hook that captures each agent's native session id
//! for resume (see `docs/plan-phase-1.md` "Hook Integration"). The daemon owns
//! the install because it runs as the same user and writes into the agent's
//! config dir (`~/.claude`, `~/.codex`).

use serde::{Deserialize, Serialize};

use crate::session::AgentKind;

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
pub const EXPECTED_INTEGRATION_VERSION: u32 = 4;

/// Parameters for `integration.install`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "IntegrationInstallParams.ts"))]
pub struct IntegrationInstallParams {
    /// Agent to install the hook for. When omitted, the daemon installs the
    /// hook for every supported agent whose config dir is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub agent: Option<AgentKind>,
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "IntegrationStatusParams.ts"))]
pub struct IntegrationStatusParams {
    /// Restrict the read-only report to one agent. When omitted, report every
    /// supported hook agent regardless of whether its config dir exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub agent: Option<AgentKind>,
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
    pub agent: AgentKind,
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
    pub agent: AgentKind,
    /// Absolute path of the installed hook script.
    pub hook_path: String,
    /// Config files the installer created or merged into (settings.json /
    /// hooks.json / config.toml), in the order they were touched.
    pub config_paths: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::{IntegrationInstallState, IntegrationRecovery, IntegrationStatusParams};

    #[test]
    fn integration_status_params_default_to_all_agents() {
        assert_eq!(IntegrationStatusParams::default().agent, None);
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
}
