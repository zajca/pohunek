//! Typed payloads for hook integration installation.
//!
//! The `integration.install` control method asks the daemon to install the
//! per-agent `SessionStart` hook that captures each agent's native session id
//! for resume (see `docs/plan-phase-1.md` "Hook Integration"). The daemon owns
//! the install because it runs as the same user and writes into the agent's
//! config dir (`~/.claude`, `~/.codex`).

use serde::{Deserialize, Serialize};

use crate::session::AgentKind;

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
