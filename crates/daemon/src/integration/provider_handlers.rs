//! The compiled integration handlers and their registry.
//!
//! The Claude and Codex handlers run their lifecycle in the daemon over the
//! rollback-capable commit machinery; the Hermes handler is registered so a
//! descriptor can name it, and its lifecycle runs in the CLI.

// Rust guideline compliant 2026-10-05

use std::path::{Path, PathBuf};

use pohunek_worker_protocol::HookAction;
use protocol::{IntegrationAgentStatus, IntegrationUninstallReport, ProtocolError, RuntimeRef};

use super::commit::StepGate;
use super::handler::{CliRunHandler, DaemonHandler, Handler, StagedUpdate};
use super::{
    claude_config_dir, codex_config_dir, reported_agent_status, stage_claude, stage_codex,
    AssetManifest, InstallPaths, StagedClaude, StagedCodex, StatusAgent, CLAUDE_REPORTED_ACTIONS,
    CODEX_REPORTED_ACTIONS,
};

/// Hook operations the Hermes asset set reports.
const HERMES_REPORTED_ACTIONS: &[HookAction] = &[
    HookAction::IdentityReport,
    HookAction::IdentityRelease,
    HookAction::Notification,
];

/// The closed handler registry, in a stable order.
pub(super) static REGISTRY: [Handler; 3] = [
    Handler::Daemon(&CodexHook),
    Handler::Daemon(&ClaudeHook),
    Handler::CliRun(&HERMES_HOOK),
];

/// Handler of the Codex hook asset set.
#[derive(Debug)]
struct CodexHook;

/// Handler of the Claude hook asset set.
#[derive(Debug)]
struct ClaudeHook;

/// Registration of the Hermes hook handler, whose lifecycle the CLI runs.
static HERMES_HOOK: CliRunHandler = CliRunHandler {
    id: "hermes-hook-v1",
    actions: HERMES_REPORTED_ACTIONS,
};

impl StagedUpdate for StagedClaude {
    fn manifest(&self) -> &AssetManifest {
        &self.manifest
    }

    fn activate(self: Box<Self>, gate: StepGate<'_>) -> Result<InstallPaths, ProtocolError> {
        self.commit_install(gate)
    }
}

impl StagedUpdate for StagedCodex {
    fn manifest(&self) -> &AssetManifest {
        &self.manifest
    }

    fn activate(self: Box<Self>, gate: StepGate<'_>) -> Result<InstallPaths, ProtocolError> {
        self.commit_install(gate)
    }
}

impl DaemonHandler for CodexHook {
    fn id(&self) -> &'static str {
        "codex-hook-v1"
    }

    fn reported_actions(&self) -> &'static [HookAction] {
        CODEX_REPORTED_ACTIONS
    }

    fn config_dir(&self) -> Result<PathBuf, ProtocolError> {
        codex_config_dir()
    }

    fn quarantine_subdirs(&self) -> &'static [&'static str] {
        &[]
    }

    fn inspect_provider(&self) -> IntegrationAgentStatus {
        reported_agent_status(StatusAgent::Codex)
    }

    fn stage(&self, dir: &Path) -> Result<Box<dyn StagedUpdate>, ProtocolError> {
        Ok(Box::new(stage_codex(dir)?))
    }

    fn uninstall(
        &self,
        runtime: &RuntimeRef,
        dir: &Path,
    ) -> Result<IntegrationUninstallReport, ProtocolError> {
        let mut report = super::uninstall::uninstall_codex(dir)?;
        report.agent = runtime.clone();
        Ok(report)
    }
}

impl DaemonHandler for ClaudeHook {
    fn id(&self) -> &'static str {
        "claude-hook-v1"
    }

    fn reported_actions(&self) -> &'static [HookAction] {
        CLAUDE_REPORTED_ACTIONS
    }

    fn config_dir(&self) -> Result<PathBuf, ProtocolError> {
        claude_config_dir()
    }

    fn quarantine_subdirs(&self) -> &'static [&'static str] {
        &["hooks"]
    }

    fn inspect_provider(&self) -> IntegrationAgentStatus {
        reported_agent_status(StatusAgent::Claude)
    }

    fn stage(&self, dir: &Path) -> Result<Box<dyn StagedUpdate>, ProtocolError> {
        Ok(Box::new(stage_claude(dir)?))
    }

    fn uninstall(
        &self,
        runtime: &RuntimeRef,
        dir: &Path,
    ) -> Result<IntegrationUninstallReport, ProtocolError> {
        let mut report = super::uninstall::uninstall_claude(dir)?;
        report.agent = runtime.clone();
        Ok(report)
    }
}
