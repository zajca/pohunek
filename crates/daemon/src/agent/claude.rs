use protocol::ProtocolError;

use super::{launch_command, AgentAdapter, InputRules, LaunchOpts, DEFAULT_CLAUDE_SUBMIT_DELAY};
use crate::detect::Manifest;
use crate::pty::PtyCommand;

/// Claude Code PTY/TUI adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeAdapter;

impl AgentAdapter for ClaudeAdapter {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn launch(&self, opts: &LaunchOpts) -> Result<PtyCommand, ProtocolError> {
        launch_command("claude", Vec::new(), opts)
    }

    fn input_rules(&self) -> InputRules {
        InputRules {
            bracketed_paste: false,
            submit_delay: DEFAULT_CLAUDE_SUBMIT_DELAY,
        }
    }

    fn manifest(&self) -> &Manifest {
        crate::detect::claude_manifest()
    }
}
