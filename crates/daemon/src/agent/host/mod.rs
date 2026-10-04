//! Data-driven agent runtime host: definitions, sources and the registry.
//!
//! A [`RuntimeRegistry`] resolves a protocol [`protocol::RuntimeId`] to an
//! immutable [`RuntimeDefinition`]. Definitions come from [`RuntimeSource`]s;
//! the [`BuiltinSource`] supplies the shell, Codex, Claude and Hermes
//! descriptors, which reserve their ids against every other source.

mod builtin;
mod definition;
#[cfg(test)]
pub(crate) mod fixture;
mod handle;
mod launch;
mod registry;
mod source;

pub use builtin::BuiltinSource;
pub use definition::{
    DefinitionError, DefinitionInvariant, DefinitionOrigin, DefinitionParts, HandlerId,
    LaunchProgram, RuntimeDefinition, MAX_ARG_BYTES, MAX_DEFINITION_BYTES, MAX_LABEL_BYTES,
    MAX_LAUNCH_ARGS, SUPPORTED_RUNTIME_API, SUPPORTED_SCHEMA,
};
pub use handle::RuntimeHost;
pub use launch::LaunchPin;
pub(crate) use launch::{check_pin, launch_command, validate_launch_runtime};
pub use registry::{
    InventoryEntry, RegistryError, RuntimeRegistry, RuntimeSource, SourceTrust,
    RESERVED_RUNTIME_IDS,
};
pub use source::{LaunchSource, ProfileRevision};
pub(crate) use source::{ProfileInputs, RevisionKeys};

#[cfg(test)]
mod tests;
