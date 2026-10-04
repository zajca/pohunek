//! The runtimes compiled into the daemon.
//!
//! Codex, Claude and Hermes are parsed from embedded `runtime.toml`
//! descriptors. The shell has no package identity and no native resume, so it
//! is assembled from explicit parts around the host's login shell.

use std::sync::Arc;
use std::time::Duration;

use protocol::{AgentKind, RuntimeId};

use super::definition::{
    DefinitionError, DefinitionOrigin, DefinitionParts, LaunchProgram, RuntimeDefinition,
};
use super::registry::{RuntimeSource, SourceTrust};
use crate::agent::{default_program, InputRules};
use crate::detect::{self, Manifest};

/// Embedded descriptors of the built-in agent runtimes.
const EMBEDDED_DESCRIPTORS: [&str; 3] = [
    include_str!("../builtin/codex.toml"),
    include_str!("../builtin/claude.toml"),
    include_str!("../builtin/hermes.toml"),
];

/// Display name of the shell runtime.
const SHELL_DISPLAY_NAME: &str = "Shell";

/// Supplies the shell, Codex, Claude and Hermes definitions.
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

    /// Creates a source whose shell runtime launches the host's login shell.
    #[must_use]
    pub fn from_host_environment() -> Self {
        Self::new(default_program(&AgentKind::Shell))
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
            submit_delay_configurable: false,
            manifest: embedded_manifest("shell")?,
            native: None,
            prompt_arg: false,
            version_probe_parser: None,
            integration_handler: None,
        })
    }
}

impl RuntimeSource for BuiltinSource {
    fn trust(&self) -> SourceTrust {
        SourceTrust::Builtin
    }

    fn load(&self) -> Result<Vec<RuntimeDefinition>, DefinitionError> {
        let mut definitions = vec![self.shell_definition()?];
        for source in EMBEDDED_DESCRIPTORS {
            definitions.push(RuntimeDefinition::from_toml(
                source,
                |package| DefinitionOrigin::Builtin {
                    package: Some(package),
                },
                embedded_manifest,
            )?);
        }
        Ok(definitions)
    }
}

/// Resolves the name of a detection manifest embedded in the daemon.
fn embedded_manifest(name: &str) -> Result<Arc<Manifest>, DefinitionError> {
    let manifest = match name {
        "shell" => detect::generic_shell_manifest(),
        "codex" => detect::codex_manifest(),
        "claude" => detect::claude_manifest(),
        "hermes" => detect::hermes_manifest(),
        _ => return Err(DefinitionError::UnknownManifest),
    };
    Ok(Arc::new(manifest.clone()))
}
