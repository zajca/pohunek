use protocol::ProtocolError;

use super::LaunchCommand;
use super::{launch_command, AgentAdapter, InputRules, LaunchOpts, DEFAULT_CLAUDE_SUBMIT_DELAY};
use crate::detect::Manifest;

/// Claude Code PTY/TUI adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeAdapter;

impl AgentAdapter for ClaudeAdapter {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn launch(&self, opts: &LaunchOpts) -> Result<LaunchCommand, ProtocolError> {
        launch_command("claude", Vec::new(), opts)
    }

    fn input_rules(&self) -> InputRules {
        InputRules::unrestricted(false, DEFAULT_CLAUDE_SUBMIT_DELAY)
    }

    fn manifest(&self) -> &Manifest {
        crate::detect::claude_manifest()
    }
}
