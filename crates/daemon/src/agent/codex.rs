use std::time::Duration;

use protocol::ProtocolError;

use super::{
    launch_command, resume_command, AgentAdapter, AgentCommand, InputRules, LaunchOpts, SessionRef,
};
use crate::detect::Manifest;
use crate::pty::PtyCommand;

/// Codex PTY/TUI adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodexAdapter;

impl AgentAdapter for CodexAdapter {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn launch(&self, opts: &LaunchOpts) -> Result<PtyCommand, ProtocolError> {
        launch_command("codex", Vec::new(), opts)
    }

    fn input_rules(&self) -> InputRules {
        InputRules {
            bracketed_paste: true,
            submit_delay: Duration::ZERO,
        }
    }

    fn manifest(&self) -> &'static Manifest {
        crate::detect::codex_manifest()
    }

    fn resume(&self, session_ref: &SessionRef) -> AgentCommand {
        resume_command(
            "codex",
            vec!["resume".to_owned(), session_ref.value().to_owned()],
        )
    }
}
