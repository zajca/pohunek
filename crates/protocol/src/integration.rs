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
/// Wire protocol version the hook must stamp on its request envelope.
///
/// Injected (rather than baked into the asset) so the hook never hardcodes the
/// protocol version: [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION) is the single
/// source of truth.
pub const ENV_PROTOCOL_VERSION: &str = "POHUNEK_PROTOCOL_VERSION";

/// Parameters for `integration.install`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationInstallParams {
    /// Agent to install the hook for. When omitted, the daemon installs the
    /// hook for every supported agent whose config dir is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentKind>,
}

/// Result returned by `integration.install`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationInstallResult {
    /// One report per agent the hook was installed for.
    pub installed: Vec<IntegrationInstallReport>,
}

/// Per-agent record of what the installer wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationInstallReport {
    /// Agent the hook was installed for.
    pub agent: AgentKind,
    /// Absolute path of the installed hook script.
    pub hook_path: String,
    /// Config files the installer created or merged into (settings.json /
    /// hooks.json / config.toml), in the order they were touched.
    pub config_paths: Vec<String>,
}
