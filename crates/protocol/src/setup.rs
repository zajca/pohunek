//! Typed payloads of `package.setup_resolve` and the setup entries a package
//! descriptor exports.
//!
//! A setup entry is an interactive program of a package that the owner runs
//! from a local terminal with `pohunek plugin run`. The daemon never runs it:
//! it only resolves the entry to a verified program path, and the CLI starts
//! that program on the owner's terminal. The request names only the package
//! and the entry; the arguments the owner types never reach the daemon.
//!
//! Every error is fixed text: none carries a path or a caller-supplied value.

// Rust guideline compliant 2026-11-23

use serde::{Deserialize, Serialize};

use crate::{CommandName, PackageDigest, PackageId, PackageIdentity};

/// One setup entry a package's descriptor exports, for review before and after
/// installation.
///
/// The program and the fixed arguments are what `pohunek plugin run` starts on
/// the owner's terminal, followed by the arguments the owner types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SetupInfo.ts"))]
pub struct SetupInfo {
    /// Entry name, unique within the package.
    pub name: CommandName,
    /// Package-relative path of the executable the entry runs.
    pub program: String,
    /// Fixed arguments placed before the owner's arguments, passed literally.
    ///
    /// An argument may contain Unicode format characters (bidirectional
    /// controls, zero-width characters), which the descriptor checks accept. A
    /// consumer that shows the arguments for consent escapes them first, as
    /// the `pohunek` CLI does by printing each argument in its debug form.
    pub args: Vec<String>,
    /// One line that says what the entry does, shown in the install review.
    pub description: String,
}

/// Parameters for `package.setup_resolve`.
///
/// Unknown fields are refused, so a request cannot carry an argument or an
/// environment variable to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SetupResolveParams.ts"))]
pub struct SetupResolveParams {
    /// Package that declares the entry.
    pub package: PackageId,
    /// Setup entry to resolve.
    pub setup: CommandName,
}

/// Result of `package.setup_resolve`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts", ts(export, export_to = "SetupResolveResult.ts"))]
pub struct SetupResolveResult {
    /// Id and version of the package version that resolved.
    pub package: PackageIdentity,
    /// Setup entry that resolved.
    pub setup: CommandName,
    /// Digest of the package version whose root was verified.
    pub digest: PackageDigest,
    /// Absolute path of the verified package root.
    pub root: String,
    /// Absolute path of the program, a file below `root` that the manifest
    /// marks executable.
    pub program: String,
    /// The descriptor's fixed arguments.
    pub args: Vec<String>,
}
