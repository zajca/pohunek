//! Data-driven agent runtime host: definitions, sources and the registry.
//!
//! A [`RuntimeRegistry`] resolves a protocol [`protocol::RuntimeId`] to an
//! immutable [`RuntimeDefinition`]. Definitions come from [`RuntimeSource`]s;
//! the [`BuiltinSource`] supplies the shell; local and third-party sources
//! cannot claim the reserved ids (the shell and the official aliases `codex`,
//! `claude` and `hermes`), while a catalog-authorized package is the only
//! source of an official alias.

mod builtin;
mod claim;
mod commands;
mod config_home;
mod definition;
mod features;
#[cfg(test)]
pub(crate) mod fixture;
mod handle;
mod launch;
mod launch_env;
mod package;
mod registry;
mod source;
mod transcripts;

pub use ::package::version_probe::{
    LineTemplate, LineTemplateError, ProbeVersion, VersionProbePolicy, VersionRefusal,
    MAX_PROBE_ARGS, SEMVER_LINE_PARSER_ID, SEMVER_PARSER_ID, SEMVER_REVISION_LINE_PARSER_ID,
};
pub use builtin::BuiltinSource;
pub use claim::{decide_claim, is_reserved, Authority, ClaimRefusal, ServedBy};
pub use commands::{PackageDescriptor, MAX_PACKAGE_COMMANDS};
pub use config_home::{ConfigHome, HomeError};
pub use definition::{
    DefinitionError, DefinitionInvariant, DefinitionOrigin, DefinitionParts, HandlerId,
    InputReadiness, Integration, LaunchProgram, RuntimeDefinition, MAX_ARG_BYTES,
    MAX_DEFINITION_BYTES, MAX_LABEL_BYTES, MAX_LAUNCH_ARGS, SUPPORTED_RUNTIME_API,
    SUPPORTED_SCHEMA,
};
pub use handle::{PackageReport, ReloadError, RuntimeHost};
#[cfg(test)]
pub(crate) use launch::check_pin;
pub use launch::LaunchPin;
pub(crate) use launch::{
    adopts, adopts_newer, honors_frozen_launch, launch_command, relaunch_launch, relaunch_pin,
    validate_launch_runtime, validate_launch_runtime_with_timeout_detailed, version_order, Spawn,
};
pub use launch_env::{
    DescriptorLaunchEnv, GLUED_ENDINGS, LAUNCH_ENV_VALUES, LOCATION_WORDS, MAX_LAUNCH_ENV_ENTRIES,
    REFUSED_NAMESPACES, SECRET_WORDS,
};
pub(crate) use package::may_serve_reserved;
pub use package::{
    definition_from_archive, descriptor_from_archive, PackageLoad, PackageRejection, PackageSource,
    PackageStore, RejectedPackage, RUNTIME_DESCRIPTOR_PATH,
};
pub use registry::{
    InventoryEntry, RegistryError, RuntimeRegistry, RuntimeSource, SourceTrust,
    RESERVED_RUNTIME_IDS,
};
pub use source::{LaunchSource, ProfileRevision};
pub(crate) use source::{ProfileInputs, RevisionKeys};
pub use transcripts::{
    TranscriptParser, Transcripts, JSONL_HEAD_V1_PARSER_ID, MAX_TRANSCRIPT_DEPTH,
    MAX_TRANSCRIPT_DIR_BYTES, MAX_TRANSCRIPT_DIR_COMPONENTS, MUSE_SESSION_PARSER_ID,
};

#[cfg(test)]
mod package_tests;
#[cfg(test)]
mod tests;
