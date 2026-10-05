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

/// Command-line flag the recovery commands of a status warning select a runtime with.
const AGENT_FLAG: &str = "--agent";

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

/// Reports `agent`'s status for `runtime`.
///
/// The status inspection words its recovery commands with the provider's own
/// agent name; they are rewritten to name `runtime`, the id the operator
/// passes to `--agent`.
fn inspect_for(agent: StatusAgent, runtime: &RuntimeRef) -> IntegrationAgentStatus {
    let mut status = reported_agent_status(agent);
    let provider = agent.kind();
    if provider != *runtime {
        let from = format!("{AGENT_FLAG} {}", provider.as_wire());
        let to = format!("{AGENT_FLAG} {}", runtime.as_wire());
        for warning in &mut status.warnings {
            *warning = warning.replace(&from, &to);
        }
    }
    status.agent = runtime.clone();
    status
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

    fn inspect(&self, runtime: &RuntimeRef) -> IntegrationAgentStatus {
        inspect_for(StatusAgent::Codex, runtime)
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

    fn inspect(&self, runtime: &RuntimeRef) -> IntegrationAgentStatus {
        inspect_for(StatusAgent::Claude, runtime)
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
