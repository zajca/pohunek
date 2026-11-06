//! The compiled integration handlers and their registry.
//!
//! The Claude and Codex handlers run their lifecycle in the daemon over the
//! rollback-capable commit machinery; the Hermes handler is registered so a
//! descriptor can name it, and its lifecycle runs in the CLI.

// Rust guideline compliant 2026-10-12

use std::path::Path;

use pohunek_worker_protocol::HookAction;
use protocol::{IntegrationAgentStatus, IntegrationUninstallReport, ProtocolError, RuntimeRef};

use super::commit::StepGate;
use super::handler::{CliRunHandler, DaemonHandler, Handler, RetiredBuiltin, StagedUpdate};
use super::launch_input::{LaunchContributor, LaunchInput, LaunchInputContext};
use super::{
    reported_agent_status, stage_claude, stage_codex, AssetManifest, InstallPaths, StagedClaude,
    StagedCodex, StatusAgent, CLAUDE_REPORTED_ACTIONS, CODEX_REPORTED_ACTIONS,
};

/// Hook operations the Hermes asset set reports.
const HERMES_REPORTED_ACTIONS: &[HookAction] = &[
    HookAction::IdentityReport,
    HookAction::IdentityRelease,
    HookAction::Notification,
];

/// The closed handler registry, in a stable order.
pub(super) static REGISTRY: [Handler; 5] = [
    Handler::Daemon(&CodexHook),
    Handler::Daemon(&ClaudeHook),
    Handler::CliRun(&HERMES_HOOK),
    Handler::Daemon(&super::muse::MUSE_HOOK),
    Handler::Daemon(&super::cline::CLINE_HOOK),
];

/// Handler of the Codex hook asset set.
#[derive(Debug)]
struct CodexHook;

/// The Codex argument that keeps a session's app-server a child of its own
/// TUI. Without it every Codex session on one `CODEX_HOME` attaches to the
/// first session's managed daemon, so its hooks carry the first session's
/// environment and stopping the first session strands the others.
const NO_DAEMON_ARG: &str = "--no-daemon";

impl LaunchContributor for CodexHook {
    /// Adds [`NO_DAEMON_ARG`] to every launch unless the frozen
    /// arguments already carry it, so a profile that sets it itself is not
    /// given a second copy.
    fn contribute(&self, ctx: &LaunchInputContext<'_>) -> Result<LaunchInput, ProtocolError> {
        let present = ctx.args.iter().any(|arg| arg == NO_DAEMON_ARG);
        Ok(LaunchInput {
            root_args: if present {
                Vec::new()
            } else {
                vec![NO_DAEMON_ARG.to_owned()]
            },
            ..LaunchInput::default()
        })
    }
}

/// The built-in Codex runtime release 0.34.2 compiled in, as its descriptor
/// declared it (`compat/codex/builtin-v0.34/runtime.toml`), so the hooks it
/// installed into `~/.codex` stay removable on a host without the package.
static CODEX_RETIRED: RetiredBuiltin = RetiredBuiltin {
    runtime_id: "codex",
    display_name: "Codex",
    config_env: "CODEX_HOME",
    config_default: ".codex",
    hook_schema: "identity-subagent-v1",
};

/// Handler of the Claude hook asset set.
#[derive(Debug)]
struct ClaudeHook;

/// The built-in Claude runtime release 0.34.2 compiled in, as its descriptor
/// declared it (`compat/claude/builtin-v0.34/runtime.toml`), so the hooks it
/// installed into `~/.claude` stay removable on a host without the package.
static CLAUDE_RETIRED: RetiredBuiltin = RetiredBuiltin {
    runtime_id: "claude",
    display_name: "Claude Code",
    config_env: "CLAUDE_CONFIG_DIR",
    config_default: ".claude",
    hook_schema: "identity-subagent-v1",
};

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

/// First integration version whose `Stop` hook repeats the identity report.
const STOP_IDENTITY_INTEGRATION_VERSION: u32 = 16;

/// What a Claude install older than [`STOP_IDENTITY_INTEGRATION_VERSION`] lacks;
/// the version it names is that constant.
const STOP_IDENTITY_NOTE: &str = "Until then a Claude session whose SessionStart hook was killed before it reported keeps no native reference, so resume and fork refuse it; hooks of integration version 16 or newer recover it at the end of the next turn.";

impl DaemonHandler for CodexHook {
    fn id(&self) -> &'static str {
        "codex-hook-v1"
    }

    fn reported_actions(&self) -> &'static [HookAction] {
        CODEX_REPORTED_ACTIONS
    }

    fn quarantine_subdirs(&self) -> &'static [&'static str] {
        &[]
    }

    fn inspect_provider(&self, home: Result<&Path, &ProtocolError>) -> IntegrationAgentStatus {
        reported_agent_status(StatusAgent::Codex, home)
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

    fn retired_builtin(&self) -> Option<&'static RetiredBuiltin> {
        Some(&CODEX_RETIRED)
    }

    fn launch_contributor(&self) -> Option<&'static dyn LaunchContributor> {
        Some(&CodexHook)
    }
}

impl DaemonHandler for ClaudeHook {
    fn id(&self) -> &'static str {
        "claude-hook-v1"
    }

    fn reported_actions(&self) -> &'static [HookAction] {
        CLAUDE_REPORTED_ACTIONS
    }

    fn quarantine_subdirs(&self) -> &'static [&'static str] {
        &["hooks"]
    }

    fn inspect_provider(&self, home: Result<&Path, &ProtocolError>) -> IntegrationAgentStatus {
        reported_agent_status(StatusAgent::Claude, home)
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

    fn retired_builtin(&self) -> Option<&'static RetiredBuiltin> {
        Some(&CLAUDE_RETIRED)
    }

    fn outdated_note(&self, installed_version: u32) -> Option<&'static str> {
        (installed_version < STOP_IDENTITY_INTEGRATION_VERSION).then_some(STOP_IDENTITY_NOTE)
    }
}
