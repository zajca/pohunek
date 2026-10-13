//! The `workflow.toml` manifest of a workflow plugin archive.
//!
//! The manifest names the plugin and declares its actions as argv arrays that
//! run without a shell. Parsing is strict: the document is size-capped before
//! it is parsed, every table refuses unknown keys, and every `argv[0]` names
//! either an executable file of the same archive by a package path or an
//! absolute host path. Tables of later workflow slices are unknown keys here.

// Rust guideline compliant 2026-10-13

use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::path::Path;

use package::VerifiedArchive;
use pohunek_platform::supervisor::{MAX_JOB_ARGUMENTS, MAX_JOB_VALUE_BYTES};
use protocol::{PackageId, PackageIdentity, PackageVersion};
use serde::{Deserialize, Serialize};

/// Archive path of the manifest.
pub(crate) const MANIFEST_PATH: &str = "workflow.toml";

/// Archive path of a runtime package descriptor, which a workflow plugin
/// archive must not carry: one archive is never both kinds.
const RUNTIME_DESCRIPTOR_PATH: &str = "runtime.toml";

/// Largest accepted manifest: 64 KiB, checked before parsing.
///
/// The same ceiling as a runtime descriptor (`MAX_DEFINITION_BYTES` in the
/// daemon). A manifest holds a few short argv arrays; the cap keeps a hostile
/// document from growing the parser's buffers.
pub(crate) const MAX_MANIFEST_BYTES: usize = 64 * 1024;

/// The manifest schema this build reads.
const SCHEMA: u32 = 1;

/// Longest action name: a letter followed by at most 31 of `[a-z0-9_]`.
///
/// Names become CLI arguments and, in later slices, part of service unit
/// names, so they stay short and shell-safe.
const MAX_ACTION_NAME_BYTES: usize = 32;

/// Longest plugin description: 1 KiB.
///
/// The description is one line of the consent review and `inspect`; the cap
/// keeps an archive from flooding the terminal the owner reviews it in.
const MAX_DESCRIPTION_BYTES: usize = 1024;

/// Whether an action accepts extra arguments from the command line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ArgsMode {
    /// Extra arguments are refused.
    #[default]
    None,
    /// Extra arguments are appended to argv verbatim.
    Passthrough,
}

/// One declared action.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Action {
    /// Program and fixed arguments; `argv[0]` is a package path or an
    /// absolute host path.
    pub(crate) argv: Vec<String>,
    /// Whether extra command-line arguments are accepted.
    #[serde(default)]
    pub(crate) args: ArgsMode,
    /// Requested run deadline; enforced against owner limits when actions run.
    pub(crate) timeout_ms: Option<NonZeroU64>,
    /// Requested output cap; enforced against owner limits when actions run.
    pub(crate) max_output_bytes: Option<NonZeroU64>,
}

impl Action {
    /// argv with a package-path `argv[0]` resolved below `files`, the
    /// extracted tree of the package.
    pub(crate) fn resolved_argv(&self, files: &Path) -> Vec<String> {
        self.argv
            .iter()
            .enumerate()
            .map(|(index, arg)| {
                if index == 0 && !arg.starts_with('/') {
                    files.join(arg).display().to_string()
                } else {
                    arg.clone()
                }
            })
            .collect()
    }
}

/// A validated manifest.
#[derive(Debug, Clone)]
pub(crate) struct Manifest {
    /// Plugin id and version, the identity the registry records.
    pub(crate) identity: PackageIdentity,
    /// One-line description.
    pub(crate) description: String,
    /// Actions by name.
    pub(crate) actions: BTreeMap<String, Action>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    /// Checked through [`Header`] first; kept so `deny_unknown_fields`
    /// accepts the key.
    #[serde(rename = "schema")]
    _schema: u32,
    plugin: Plugin,
    actions: BTreeMap<String, Action>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Plugin {
    id: PackageId,
    version: PackageVersion,
    description: String,
}

/// Only the schema field, so an unknown version is told apart from damage.
#[derive(Deserialize)]
struct Header {
    schema: u32,
}

/// Why a manifest was refused.
///
/// Reasons are fixed text; the only document-derived parts are a line number
/// and an action name that already matched the name grammar.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Invalid {
    /// The archive has no manifest.
    #[error("the archive has no {MANIFEST_PATH}")]
    Missing,
    /// The archive also carries a runtime descriptor.
    #[error("the archive also carries {RUNTIME_DESCRIPTOR_PATH}; a workflow plugin is not a runtime package")]
    RuntimeDescriptor,
    /// The manifest exceeds [`MAX_MANIFEST_BYTES`].
    #[error("{MANIFEST_PATH} exceeds {MAX_MANIFEST_BYTES} bytes")]
    TooLarge,
    /// The manifest is not TOML of the schema: syntax, an unknown key, a
    /// missing field or a wrong type.
    #[error("{MANIFEST_PATH} does not match the manifest schema (syntax, unknown key, missing field or wrong type){}", line.map(|line| format!(" at line {line}")).unwrap_or_default())]
    Schema {
        /// One-based line of the fault, when the parser reports one.
        line: Option<usize>,
    },
    /// The manifest declares another schema version.
    #[error("{MANIFEST_PATH} declares schema {0}; this build reads schema {SCHEMA}")]
    UnsupportedSchema(u32),
    /// The description is empty or too long.
    #[error("the plugin description must be 1..={MAX_DESCRIPTION_BYTES} bytes")]
    Description,
    /// The manifest declares no action.
    #[error("{MANIFEST_PATH} declares no action")]
    NoActions,
    /// An action name breaks the grammar; the name is not echoed.
    #[error("an action name is not [a-z][a-z0-9_]{{0,31}}")]
    ActionName,
    /// An action's argv breaks a rule.
    #[error("action {action}: {fault}")]
    Argv {
        /// The action name, valid under the name grammar.
        action: String,
        /// The broken rule.
        fault: ArgvFault,
    },
}

/// Which argv rule an action breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ArgvFault {
    /// argv has no program.
    #[error("argv is empty")]
    Empty,
    /// argv has more than [`MAX_JOB_ARGUMENTS`] entries.
    #[error("argv has more than {MAX_JOB_ARGUMENTS} entries")]
    TooMany,
    /// An entry exceeds [`MAX_JOB_VALUE_BYTES`].
    #[error("an argv entry exceeds {MAX_JOB_VALUE_BYTES} bytes")]
    TooLong,
    /// An entry contains a NUL byte.
    #[error("an argv entry contains a NUL byte")]
    Nul,
    /// `argv[0]` has no `/`: no `PATH` is searched, so it would be ambiguous.
    #[error("argv[0] is a bare command name; use a package path such as bin/tool or an absolute host path")]
    Bare,
    /// `argv[0]` has `.` or `..` segments, an empty segment or a trailing `/`.
    #[error("argv[0] is not a normalized path (`.`, `..` or empty segments)")]
    NotNormalized,
    /// An absolute `argv[0]` names no file, such as `/`.
    #[error("argv[0] is an absolute path without a file name")]
    NoFileName,
    /// The package path is not a file of the archive.
    #[error("argv[0] is not a file of the archive")]
    NotInArchive,
    /// The package path is a file without the executable bit.
    #[error("argv[0] is not executable in the archive")]
    NotExecutable,
}

/// Validates the manifest of a verified archive against the archive's files.
///
/// # Errors
///
/// Returns [`Invalid`] when the archive carries a runtime descriptor, has no
/// manifest, or the manifest breaks a rule.
pub(crate) fn from_archive(archive: &VerifiedArchive) -> Result<Manifest, Invalid> {
    let entries = archive.entries();
    // Entries are in ascending path order, the canonical archive order.
    let find = |path: &str| {
        entries
            .binary_search_by(|entry| entry.path.as_str().cmp(path))
            .ok()
            .map(|index| &entries[index])
    };
    if find(RUNTIME_DESCRIPTOR_PATH).is_some() {
        return Err(Invalid::RuntimeDescriptor);
    }
    let manifest = find(MANIFEST_PATH).ok_or(Invalid::Missing)?;
    parse(&manifest.contents, |path| {
        find(path).map(|entry| entry.executable)
    })
}

/// Parses manifest bytes; `executable` answers whether a package path is an
/// archive file and whether it carries the executable bit.
///
/// # Errors
///
/// Returns [`Invalid`] for the first broken rule.
pub(crate) fn parse(
    bytes: &[u8],
    executable: impl Fn(&str) -> Option<bool>,
) -> Result<Manifest, Invalid> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(Invalid::TooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_not_utf8| Invalid::Schema { line: None })?;
    // Parser messages can quote document values, so only the line survives.
    let schema_error = |error: toml_edit::de::Error| Invalid::Schema {
        line: error
            .span()
            .and_then(|span| text.get(..span.start))
            .map(|prefix| prefix.matches('\n').count() + 1),
    };
    let header: Header = toml_edit::de::from_str(text).map_err(schema_error)?;
    if header.schema != SCHEMA {
        return Err(Invalid::UnsupportedSchema(header.schema));
    }
    let document: Document = toml_edit::de::from_str(text).map_err(schema_error)?;
    let description = document.plugin.description;
    if description.is_empty() || description.len() > MAX_DESCRIPTION_BYTES {
        return Err(Invalid::Description);
    }
    if document.actions.is_empty() {
        return Err(Invalid::NoActions);
    }
    for (name, action) in &document.actions {
        if !valid_action_name(name) {
            return Err(Invalid::ActionName);
        }
        check_argv(&action.argv, &executable).map_err(|fault| Invalid::Argv {
            action: name.clone(),
            fault,
        })?;
    }
    Ok(Manifest {
        identity: PackageIdentity {
            id: document.plugin.id,
            version: document.plugin.version,
        },
        description,
        actions: document.actions,
    })
}

/// Whether `name` matches `[a-z][a-z0-9_]{0,31}`.
fn valid_action_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    name.len() <= MAX_ACTION_NAME_BYTES
        && bytes.next().is_some_and(|first| first.is_ascii_lowercase())
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn check_argv(argv: &[String], executable: impl Fn(&str) -> Option<bool>) -> Result<(), ArgvFault> {
    let program = argv.first().ok_or(ArgvFault::Empty)?;
    if argv.len() > MAX_JOB_ARGUMENTS {
        return Err(ArgvFault::TooMany);
    }
    if argv.iter().any(|arg| arg.len() > MAX_JOB_VALUE_BYTES) {
        return Err(ArgvFault::TooLong);
    }
    if argv.iter().any(|arg| arg.contains('\0')) {
        return Err(ArgvFault::Nul);
    }
    if program.starts_with('/') {
        let path = Path::new(program);
        if !pohunek_paths::is_normalized_absolute(path) {
            return Err(ArgvFault::NotNormalized);
        }
        return if path.file_name().is_some() {
            Ok(())
        } else {
            Err(ArgvFault::NoFileName)
        };
    }
    if !program.contains('/') {
        return Err(ArgvFault::Bare);
    }
    if program
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(ArgvFault::NotNormalized);
    }
    match executable(program) {
        None => Err(ArgvFault::NotInArchive),
        Some(false) => Err(ArgvFault::NotExecutable),
        Some(true) => Ok(()),
    }
}
