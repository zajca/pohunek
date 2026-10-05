//! Data-driven agent runtime host: definitions, sources and the registry.
//!
//! A [`RuntimeRegistry`] resolves a protocol [`protocol::RuntimeId`] to an
//! immutable [`RuntimeDefinition`]. Definitions come from [`RuntimeSource`]s;
//! the [`BuiltinSource`] supplies the shell, Codex, Claude and Hermes
//! descriptors; local and third-party sources cannot claim their ids, while a
//! catalog-authorized package serves one in place of its built-in.

mod builtin;
mod claim;
mod config_home;
mod definition;
#[cfg(test)]
pub(crate) mod fixture;
mod handle;
mod launch;
mod package;
mod registry;
mod source;
mod version_probe;

pub use builtin::BuiltinSource;
pub use claim::{decide_claim, is_reserved, Authority, ClaimRefusal, ServedBy};
pub use config_home::{ConfigHome, HomeError};
pub use definition::{
    DefinitionError, DefinitionInvariant, DefinitionOrigin, DefinitionParts, HandlerId,
    Integration, LaunchProgram, RuntimeDefinition, MAX_ARG_BYTES, MAX_DEFINITION_BYTES,
    MAX_LABEL_BYTES, MAX_LAUNCH_ARGS, SUPPORTED_RUNTIME_API, SUPPORTED_SCHEMA,
};
pub use handle::{PackageReport, ReloadError, RuntimeHost};
#[cfg(test)]
pub(crate) use launch::check_pin;
pub use launch::LaunchPin;
pub(crate) use launch::{launch_command, validate_launch_runtime};
pub(crate) use package::may_serve_reserved;
pub use package::{
    definition_from_archive, PackageLoad, PackageRejection, PackageSource, PackageStore,
    RejectedPackage, RUNTIME_DESCRIPTOR_PATH,
};
pub use registry::{
    InventoryEntry, RegistryError, RuntimeRegistry, RuntimeSource, SourceTrust,
    RESERVED_RUNTIME_IDS,
};
pub use source::{LaunchSource, ProfileRevision};
pub(crate) use source::{ProfileInputs, RevisionKeys};
pub use version_probe::{
    LineTemplate, LineTemplateError, ProbeVersion, VersionProbePolicy, MAX_PROBE_ARGS,
    SEMVER_LINE_PARSER_ID, SEMVER_PARSER_ID,
};

#[cfg(test)]
mod package_tests;
#[cfg(test)]
mod tests;
