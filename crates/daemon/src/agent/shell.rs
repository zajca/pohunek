use std::time::Duration;

use protocol::ProtocolError;

use super::{AgentAdapter, InputRules, LaunchOpts};
use crate::detect::Manifest;
use crate::pty::PtyCommand;
use crate::session::ShellCommand;

/// Generic shell PTY adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct ShellAdapter;

impl AgentAdapter for ShellAdapter {
    fn id(&self) -> &'static str {
        "shell"
    }

    fn launch(&self, opts: &LaunchOpts) -> Result<PtyCommand, ProtocolError> {
        ShellCommand::default().launch(opts)
    }

    fn input_rules(&self) -> InputRules {
        InputRules {
            bracketed_paste: false,
            submit_delay: Duration::ZERO,
        }
    }

    fn manifest(&self) -> &Manifest {
        crate::detect::generic_shell_manifest()
    }
}
