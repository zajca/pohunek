//! `pohunek integration install` — install per-agent `SessionStart` hooks.
//!
//! The hook captures each agent's native session id so a session can be resumed
//! after a daemon restart (see `docs/plan-phase-1.md` "Hook Integration"). The
//! daemon performs the install: it runs as the same user, owns the handshake
//! env names and the `session.report_native_id` method, and writes into the
//! agent's config dir. The CLI is a thin client that forwards the request.

use std::fmt::Write as _;

use clap::ValueEnum;
use protocol::{method, AgentKind, IntegrationInstallParams, IntegrationInstallResult};

use crate::client::Client;
use crate::error::CliError;
use crate::paths::Paths;
use crate::target::LOCAL_HOST;

/// Agent selector accepted by `integration install --agent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum HookAgentArg {
    /// Install the Claude Code hook.
    Claude,
    /// Install the Codex hook.
    Codex,
}

impl From<HookAgentArg> for AgentKind {
    fn from(value: HookAgentArg) -> Self {
        match value {
            HookAgentArg::Claude => AgentKind::Claude,
            HookAgentArg::Codex => AgentKind::Codex,
        }
    }
}

/// Run `integration install`.
///
/// # Errors
///
/// Returns [`CliError`] if the daemon is unreachable, rejects the request (e.g.
/// the agent's config dir is absent), or returns an unexpected payload.
pub(crate) async fn run_install(
    paths: &Paths,
    agent: Option<HookAgentArg>,
    json: bool,
) -> Result<(), CliError> {
    let params = IntegrationInstallParams {
        agent: agent.map(Into::into),
    };
    // Installing hooks is inherently a *local* daemon operation: it writes into
    // this machine's agent config dirs. Always use the local transport regardless
    // of any `--host` flag.
    let mut client = Client::connect(LOCAL_HOST, paths).await?;
    let result: IntegrationInstallResult =
        client.call::<method::IntegrationInstall>(params).await?;

    if json {
        print!("{}", crate::commands::render_json(&result)?);
    } else {
        print!("{}", render_install_human(&result));
    }
    Ok(())
}

fn agent_label(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::Shell => "shell",
        AgentKind::Codex => "codex",
        AgentKind::Claude => "claude",
    }
}

fn render_install_human(result: &IntegrationInstallResult) -> String {
    if result.installed.is_empty() {
        return "no agent hooks installed\n".to_owned();
    }
    let mut output = String::new();
    for report in &result.installed {
        let _ = writeln!(
            output,
            "installed {} hook: {}",
            agent_label(report.agent),
            report.hook_path
        );
        for path in &report.config_paths {
            let _ = writeln!(output, "  config: {path}");
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use protocol::{AgentKind, IntegrationInstallReport, IntegrationInstallResult};

    use super::{render_install_human, HookAgentArg};

    #[test]
    fn hook_agent_arg_maps_to_agent_kind() {
        assert_eq!(AgentKind::from(HookAgentArg::Claude), AgentKind::Claude);
        assert_eq!(AgentKind::from(HookAgentArg::Codex), AgentKind::Codex);
    }

    #[test]
    fn renders_install_reports_with_hook_and_config_paths() {
        let result = IntegrationInstallResult {
            installed: vec![
                IntegrationInstallReport {
                    agent: AgentKind::Claude,
                    hook_path: "/home/u/.claude/hooks/pohunek-agent-state.sh".to_owned(),
                    config_paths: vec!["/home/u/.claude/settings.json".to_owned()],
                },
                IntegrationInstallReport {
                    agent: AgentKind::Codex,
                    hook_path: "/home/u/.codex/pohunek-agent-state.sh".to_owned(),
                    config_paths: vec![
                        "/home/u/.codex/hooks.json".to_owned(),
                        "/home/u/.codex/config.toml".to_owned(),
                    ],
                },
            ],
        };

        let output = render_install_human(&result);

        assert!(output
            .contains("installed claude hook: /home/u/.claude/hooks/pohunek-agent-state.sh\n"));
        assert!(output.contains("  config: /home/u/.claude/settings.json\n"));
        assert!(output.contains("installed codex hook: /home/u/.codex/pohunek-agent-state.sh\n"));
        assert!(output.contains("  config: /home/u/.codex/hooks.json\n"));
        assert!(output.contains("  config: /home/u/.codex/config.toml\n"));
    }

    #[test]
    fn renders_empty_install_result() {
        let output = render_install_human(&IntegrationInstallResult { installed: vec![] });
        assert_eq!(output, "no agent hooks installed\n");
    }

    #[test]
    fn renders_install_result_as_json_that_deserializes() {
        let result = IntegrationInstallResult {
            installed: vec![IntegrationInstallReport {
                agent: AgentKind::Claude,
                hook_path: "/home/u/.claude/hooks/pohunek-agent-state.sh".to_owned(),
                config_paths: vec!["/home/u/.claude/settings.json".to_owned()],
            }],
        };
        let doc = crate::commands::render_json(&result).expect("json doc");
        let parsed: IntegrationInstallResult =
            serde_json::from_str(&doc).expect("parse install result");
        assert_eq!(parsed, result);
    }
}
