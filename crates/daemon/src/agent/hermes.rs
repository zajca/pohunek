//! Hermes Agent PTY adapter.

use std::time::Duration;

use protocol::ProtocolError;

use super::{default_args, launch_command, AgentAdapter, InputRules, LaunchCommand, LaunchOpts};
use crate::detect::Manifest;

/// Delay between a Hermes bracketed paste and the separate submit byte.
///
/// Hermes Agent 0.20.0 accepts bracketed paste in both its classic
/// `prompt_toolkit` interface and alternate-screen Ink TUI, and binds Enter as
/// submit separately. Keeping the writes apart prevents the submit from being
/// consumed as pasted text while preserving a multiline prompt as one input.
const HERMES_SUBMIT_DELAY: Duration = Duration::from_millis(150);

/// Hermes Agent interactive-terminal adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct HermesAdapter;

impl AgentAdapter for HermesAdapter {
    fn id(&self) -> &'static str {
        "hermes"
    }

    fn launch(&self, opts: &LaunchOpts) -> Result<LaunchCommand, ProtocolError> {
        launch_command("hermes", default_args(&protocol::AgentKind::Hermes), opts)
    }

    fn input_rules(&self) -> InputRules {
        InputRules::hermes(true, HERMES_SUBMIT_DELAY)
    }

    fn manifest(&self) -> &Manifest {
        crate::detect::hermes_manifest()
    }
}
