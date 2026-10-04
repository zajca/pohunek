//! The daemon's shared handle over its runtime registry.
//!
//! One [`RuntimeHost`] is built when the session registry starts and is
//! cloned wherever a runtime identity has to be resolved: agent-name
//! resolution, launch, resume, fork and the mutation guards. It is the only
//! place that turns an [`RuntimeRef`] into a launchable [`RuntimeDefinition`],
//! so every caller answers an unknown runtime with the same stable error.
//!
//! The registry is a snapshot: every clone of a host observes the same current
//! snapshot, and a resolution hands out an owned definition that stays valid
//! after the snapshot is replaced.

use std::sync::{Arc, PoisonError, RwLock};

use protocol::{ProtocolError, RuntimeId, RuntimeRef};

use super::builtin::BuiltinSource;
use super::definition::{LaunchProgram, RuntimeDefinition};
use super::registry::RuntimeRegistry;

/// Cheap-to-clone handle over the daemon's runtime registry.
///
/// Besides the registry it carries the host's shell command. The shell
/// runtime's definition names the host login shell; the configured shell
/// command is host state that only decides what a bare shell session launches,
/// so its text is neither descriptor data nor validated or digested like
/// descriptor arguments, and it can never make the registry fail to build.
#[derive(Debug, Clone)]
pub struct RuntimeHost {
    registry: Arc<RwLock<Arc<RuntimeRegistry>>>,
    shell_command: Arc<ShellLaunch>,
}

/// Program and arguments a host-shell runtime launches with.
#[derive(Debug)]
struct ShellLaunch {
    program: String,
    args: Vec<String>,
}

impl RuntimeHost {
    /// Wraps an already built registry; a host-shell runtime launches its
    /// definition's program with no extra arguments.
    #[must_use]
    pub fn new(registry: RuntimeRegistry) -> Self {
        let program = registry
            .definitions()
            .find_map(|definition| match definition.program() {
                LaunchProgram::HostShell(program) => Some(program.clone()),
                LaunchProgram::Fixed(_) => None,
            })
            .unwrap_or_default();
        Self {
            registry: Arc::new(RwLock::new(Arc::new(registry))),
            shell_command: Arc::new(ShellLaunch {
                program,
                args: Vec::new(),
            }),
        }
    }

    /// Returns a host whose host-shell runtime launches `program` with `args`.
    #[must_use]
    pub fn with_shell_command(mut self, program: impl Into<String>, args: Vec<String>) -> Self {
        self.shell_command = Arc::new(ShellLaunch {
            program: program.into(),
            args,
        });
        self
    }

    /// The program `definition` launches: the configured shell command's
    /// program for a host-shell runtime, the descriptor's program otherwise.
    #[must_use]
    pub fn launch_program<'a>(&'a self, definition: &'a RuntimeDefinition) -> &'a str {
        match definition.program() {
            LaunchProgram::HostShell(_) => &self.shell_command.program,
            LaunchProgram::Fixed(program) => program,
        }
    }

    /// The launch arguments of `definition`: the configured shell command's
    /// arguments for a host-shell runtime, the descriptor's fixed arguments
    /// otherwise.
    #[must_use]
    pub fn launch_args(&self, definition: &RuntimeDefinition) -> Vec<String> {
        match definition.program() {
            LaunchProgram::HostShell(_) => self.shell_command.args.clone(),
            LaunchProgram::Fixed(_) => definition.default_args().to_vec(),
        }
    }

    /// Builds a host serving the built-in runtimes with the host's login shell.
    ///
    /// # Panics
    ///
    /// Panics when an embedded descriptor is invalid; unit tests parse every
    /// embedded descriptor, so this is a build defect and never a runtime
    /// condition. The login shell is sanitized, so it cannot cause it.
    #[must_use]
    pub fn from_host_environment() -> Self {
        let source = BuiltinSource::from_host_environment();
        let registry = RuntimeRegistry::from_sources(&[&source])
            .expect("embedded built-in runtime descriptors are valid");
        Self::new(registry)
    }

    /// The current registry snapshot.
    #[must_use]
    pub fn registry(&self) -> Arc<RuntimeRegistry> {
        Arc::clone(&self.registry.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Resolves a runtime identity.
    ///
    /// # Errors
    ///
    /// Returns `runtime_not_installed` when no definition is registered.
    pub fn resolve_id(
        &self,
        runtime_id: &RuntimeId,
    ) -> Result<Arc<RuntimeDefinition>, ProtocolError> {
        self.registry().resolve(runtime_id).map(Arc::clone)
    }

    /// Resolves the runtime a [`RuntimeRef`] names.
    ///
    /// # Errors
    ///
    /// Returns `agent_kind_unsupported` for a value that is not a valid runtime
    /// id and `runtime_not_installed` for a valid id no definition backs.
    pub fn resolve_ref(
        &self,
        reference: &RuntimeRef,
    ) -> Result<Arc<RuntimeDefinition>, ProtocolError> {
        self.registry()
            .resolve(reference.launchable()?)
            .map(Arc::clone)
    }
}

impl Default for RuntimeHost {
    fn default() -> Self {
        Self::from_host_environment()
    }
}
