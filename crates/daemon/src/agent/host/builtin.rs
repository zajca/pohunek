//! The runtime compiled into the daemon.
//!
//! The shell is the only built-in runtime. Codex, Claude and Hermes are not
//! compiled in; only their official runtime packages serve them. The shell has
//! no package identity and no native resume, so it is assembled from explicit
//! parts around the host's login shell.

use std::sync::Arc;
use std::time::Duration;

use protocol::RuntimeId;

use super::definition::{
    DefinitionError, DefinitionOrigin, DefinitionParts, InputReadiness, Integration, LaunchProgram,
    RuntimeDefinition,
};
use super::registry::{RuntimeSource, SourceTrust};
use crate::agent::InputRules;
use crate::detect;

/// Display name of the shell runtime.
const SHELL_DISPLAY_NAME: &str = "Shell";

/// Hook schema of the shell runtime.
///
/// A shell session can host an agent in the foreground whose runtime reports
/// through this schema (the official Codex and Claude packages do), so it
/// admits identity, notification, and subagent reports. The shell has no
/// integration handler of its own.
const SHELL_HOOK_SCHEMA: &str = "identity-subagent-v1";

/// Supplies the shell definition.
#[derive(Debug, Clone)]
pub struct BuiltinSource {
    shell_program: String,
}

impl BuiltinSource {
    /// Creates a source whose shell runtime launches `shell_program`.
    #[must_use]
    pub fn new(shell_program: impl Into<String>) -> Self {
        Self {
            shell_program: shell_program.into(),
        }
    }

    /// Creates a source whose shell runtime launches the host's login shell,
    /// resolved by [`crate::agent::host_login_shell`].
    #[must_use]
    pub fn from_host_environment() -> Self {
        Self::new(crate::agent::host_login_shell())
    }

    /// Creates a source from a raw `$SHELL` value, resolved by
    /// [`crate::agent::resolve_login_shell`].
    #[must_use]
    pub fn from_login_shell(raw: Option<String>) -> Self {
        Self::new(crate::agent::resolve_login_shell(raw))
    }

    fn shell_definition(&self) -> Result<RuntimeDefinition, DefinitionError> {
        let runtime_id =
            RuntimeId::parse(RuntimeId::SHELL).map_err(|source| DefinitionError::Identifier {
                field: "runtime.id",
                source,
            })?;
        RuntimeDefinition::new(DefinitionParts {
            runtime_id,
            origin: DefinitionOrigin::Builtin { package: None },
            display_name: SHELL_DISPLAY_NAME.to_owned(),
            program: LaunchProgram::HostShell(self.shell_program.clone()),
            default_args: Vec::new(),
            input_rules: InputRules::unrestricted(false, Duration::ZERO),
            input_readiness: InputReadiness::BracketedPaste,
            submit_delay_configurable: false,
            manifest: Arc::clone(detect::shared_generic_shell_manifest()),
            native: None,
            prompt_arg: false,
            version_probe_parser: None,
            version_probe_policy: None,
            integration: Some(Integration::new(None, SHELL_HOOK_SCHEMA)?),
        })
    }
}

impl RuntimeSource for BuiltinSource {
    fn trust(&self) -> SourceTrust {
        SourceTrust::Builtin
    }

    fn load(&self) -> Result<Vec<RuntimeDefinition>, DefinitionError> {
        Ok(vec![self.shell_definition()?])
    }
}
