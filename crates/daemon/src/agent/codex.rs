use std::time::Duration;

use protocol::ProtocolError;

use super::{
    launch_command, resume_command, AgentAdapter, AgentCommand, InputRules, LaunchOpts, SessionRef,
};
use crate::detect::Manifest;
use crate::pty::PtyCommand;

/// Delay after bracketed paste before submitting Codex input.
///
/// Codex accepts bracketed paste for multi-line prompts, but newer TUIs can
/// treat an immediate Enter in the same burst as part of paste handling instead
/// of a submit. Keeping submit as a separate write mirrors the Claude Ink guard
/// while preserving bracketed paste for prompt bodies.
const CODEX_SUBMIT_DELAY: Duration = Duration::from_millis(150);

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
            submit_delay: CODEX_SUBMIT_DELAY,
        }
    }

    fn manifest(&self) -> &Manifest {
        crate::detect::codex_manifest()
    }

    fn resume(&self, session_ref: &SessionRef) -> Result<AgentCommand, ProtocolError> {
        Ok(resume_command(
            "codex",
            vec!["resume".to_owned(), session_ref.value().to_owned()],
        ))
    }
}
