//! Data-driven agent runtime host: definitions, sources and the registry.
//!
//! A [`RuntimeRegistry`] resolves a protocol [`protocol::RuntimeId`] to an
//! immutable [`RuntimeDefinition`]. Definitions come from [`RuntimeSource`]s;
//! the [`BuiltinSource`] supplies the shell, Codex, Claude and Hermes
//! descriptors, which reserve their ids against every other source.

mod builtin;
mod definition;
mod registry;

pub use builtin::BuiltinSource;
pub use definition::{
    DefinitionError, DefinitionOrigin, DefinitionParts, HandlerId, LaunchProgram,
    RuntimeDefinition, MAX_ARG_BYTES, MAX_DEFINITION_BYTES, MAX_LABEL_BYTES, MAX_LAUNCH_ARGS,
    SUPPORTED_RUNTIME_API, SUPPORTED_SCHEMA,
};
pub use registry::{
    InventoryEntry, RegistryError, RuntimeRegistry, RuntimeSource, SourceTrust,
    RESERVED_RUNTIME_IDS,
};

#[cfg(test)]
mod tests;
