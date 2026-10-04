//! The runtimes compiled into the daemon.
//!
//! Codex, Claude and Hermes are parsed from embedded `runtime.toml`
//! descriptors. The shell has no package identity and no native resume, so it
//! is assembled from explicit parts around the host's login shell.

use std::sync::Arc;
use std::time::Duration;

use protocol::RuntimeId;

use super::definition::{
    DefinitionError, DefinitionOrigin, DefinitionParts, LaunchProgram, RuntimeDefinition,
    MAX_ARG_BYTES,
};
use super::registry::{RuntimeSource, SourceTrust};
use crate::agent::InputRules;
use crate::detect::{self, Manifest};

/// Embedded descriptors of the built-in agent runtimes.
const EMBEDDED_DESCRIPTORS: [&str; 3] = [
    include_str!("../builtin/codex.toml"),
    include_str!("../builtin/claude.toml"),
    include_str!("../builtin/hermes.toml"),
];

/// Display name of the shell runtime.
const SHELL_DISPLAY_NAME: &str = "Shell";

/// Shell launched when the host reports no usable login shell.
const FALLBACK_SHELL: &str = "/bin/sh";

/// Whether `shell` passes the same program rules a definition enforces.
fn is_valid_shell_program(shell: &str) -> bool {
    !shell.is_empty() && shell.len() <= MAX_ARG_BYTES && !shell.chars().any(char::is_control)
}

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
    ///
    /// `$SHELL` is host input, so it is used only when it satisfies the
    /// definition's program rules; otherwise the shell falls back to
    /// [`FALLBACK_SHELL`], the same value the daemon uses when `$SHELL` is unset.
    /// Building the registry therefore cannot fail because of the environment.
    #[must_use]
    pub fn from_host_environment() -> Self {
        Self::from_login_shell(std::env::var("SHELL").ok())
    }

    /// [`from_host_environment`](Self::from_host_environment) with the login
    /// shell value passed in.
    #[must_use]
    pub fn from_login_shell(login_shell: Option<String>) -> Self {
        let shell_program = login_shell
            .filter(|shell| is_valid_shell_program(shell))
            .unwrap_or_else(|| FALLBACK_SHELL.to_owned());
        Self::new(shell_program)
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
