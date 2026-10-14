//! The `workflow.toml` manifest of a workflow plugin archive.
//!
//! The manifest names the plugin and declares its actions, periodic jobs and
//! long-running services, and its event reactions as argv arrays that run
//! without a shell. Parsing is strict: the document is size-capped before it
//! is parsed, every table refuses unknown keys, and every `argv[0]` names
//! either an executable file of the same archive by a package path or an
//! absolute host path.

// Rust guideline compliant 2026-10-13

use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::path::Path;

use package::VerifiedArchive;
use pohunek_platform::supervisor::{MAX_JOB_ARGUMENTS, MAX_JOB_INTERVAL, MAX_JOB_VALUE_BYTES};
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

/// Longest action, job or service name: a letter followed by at most 31 of
/// `[a-z0-9_]`.
///
/// Names become CLI arguments and part of service manager unit names and
/// state file names, so they stay short and shell-safe; the unit name bound
/// (`MAX_PLUGIN_UNIT_NAME_BYTES`) is larger.
const MAX_ACTION_NAME_BYTES: usize = 32;

/// Longest number of events one reaction subscribes to.
///
/// The public event table holds fifteen entries, so the cap only stops a
/// manifest from listing the same events in a loop of its own making.
const MAX_REACTION_EVENTS: usize = 32;

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
        resolve_argv(&self.argv, files)
    }
}

/// One declared periodic job.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Job {
    /// Program and fixed arguments, under the rules of [`Action::argv`].
    pub(crate) argv: Vec<String>,
    /// Wall-clock seconds from the start of one run to the start of the next;
    /// enforced against the owner's `jobs.min_interval_s` when units are
    /// installed.
    pub(crate) every_s: NonZeroU64,
    /// Requested run deadline; enforced against the owner's
    /// `jobs.timeout_max_ms` when units are installed and when a run starts.
    pub(crate) timeout_ms: Option<NonZeroU64>,
}

/// One declared long-running service.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Service {
    /// Program and fixed arguments, under the rules of [`Action::argv`].
    pub(crate) argv: Vec<String>,
}

/// One declared event reaction.
///
/// A reaction runs once for every event of its `events` list that the daemon
/// publishes on the public subscription. The events are the protocol's event
/// table minus the attach stream events; see [`reactable`].
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reaction {
    /// Public event names, distinct and non-empty.
    pub(crate) events: Vec<String>,
    /// Program and fixed arguments, under the rules of [`Action::argv`].
    pub(crate) argv: Vec<String>,
    /// Requested run deadline; enforced against the owner's
    /// `action_timeout_max_ms` when a reaction runs.
    pub(crate) timeout_ms: Option<NonZeroU64>,
}

/// Whether a reaction may subscribe to the public event `name`.
///
/// Every event of the protocol's event table qualifies except the attach
/// stream events, whose payloads describe terminal streams rather than
/// session state a reaction can act on.
pub(crate) fn reactable(name: &str) -> bool {
    !matches!(
        name,
        protocol::event::ATTACH_OPENED | protocol::event::ATTACH_CLOSED
    ) && protocol::event::EVENT_SPECS
        .iter()
        .any(|spec| spec.name == name)
}

/// argv with a package-path `argv[0]` resolved below `files`, the extracted
/// tree of the package.
pub(crate) fn resolve_argv(argv: &[String], files: &Path) -> Vec<String> {
    argv.iter()
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

/// A validated manifest.
#[derive(Debug, Clone)]
pub(crate) struct Manifest {
    /// Plugin id and version, the identity the registry records.
    pub(crate) identity: PackageIdentity,
    /// One-line description.
    pub(crate) description: String,
    /// Actions by name.
    pub(crate) actions: BTreeMap<String, Action>,
    /// Periodic jobs by name.
    pub(crate) jobs: BTreeMap<String, Job>,
    /// Long-running services by name.
    pub(crate) services: BTreeMap<String, Service>,
    /// Event reactions by name.
    pub(crate) reactions: BTreeMap<String, Reaction>,
}

impl Manifest {
    /// Whether the manifest declares a job, a service or a reaction. Such a
    /// plugin needs the service manager: jobs and services run as its units,
    /// and reactions run in the namespace's dispatcher unit.
    pub(crate) fn has_units(&self) -> bool {
        !self.jobs.is_empty() || !self.services.is_empty() || !self.reactions.is_empty()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    /// Checked through [`Header`] first; kept so `deny_unknown_fields`
    /// accepts the key.
    #[serde(rename = "schema")]
    _schema: u32,
    plugin: Plugin,
    #[serde(default)]
    actions: BTreeMap<String, Action>,
    #[serde(default)]
    jobs: BTreeMap<String, Job>,
    #[serde(default)]
    services: BTreeMap<String, Service>,
    #[serde(default)]
    reactions: BTreeMap<String, Reaction>,
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
    /// The manifest declares no action, job, service or reaction.
    #[error("{MANIFEST_PATH} declares no action, job, service or reaction")]
    NoActions,
    /// An action, job, service or reaction name breaks the grammar; the name
    /// is not echoed.
    #[error("an action, job, service or reaction name is not [a-z][a-z0-9_]{{0,31}}")]
    ActionName,
    /// An action's, job's, service's or reaction's argv breaks a rule.
    #[error("{kind} {action}: {fault}")]
    Argv {
        /// `action`, `job`, `service` or `reaction`.
        kind: &'static str,
        /// The name, valid under the name grammar.
        action: String,
        /// The broken rule.
        fault: ArgvFault,
    },
    /// A job's `every_s` is above the longest interval the service managers
    /// accept.
    #[error("job {job}: every_s must be at most {} seconds", MAX_JOB_INTERVAL.as_secs())]
    Interval {
        /// The job name, valid under the name grammar.
        job: String,
    },
    /// A reaction's `events` is empty, too long, repeats a name or names an
    /// event that is not reactable.
    #[error("reaction {reaction}: events must be 1..={MAX_REACTION_EVENTS} distinct public event names, none of them attach_opened or attach_closed")]
    Events {
        /// The reaction name, valid under the name grammar.
        reaction: String,
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
    if document.actions.is_empty()
        && document.jobs.is_empty()
        && document.services.is_empty()
        && document.reactions.is_empty()
    {
        return Err(Invalid::NoActions);
    }
    let declared = (document
        .actions
        .iter()
        .map(|(name, action)| ("action", name, &action.argv)))
    .chain(
        document
            .jobs
            .iter()
            .map(|(name, job)| ("job", name, &job.argv)),
    )
    .chain(
        document
            .services
            .iter()
            .map(|(name, service)| ("service", name, &service.argv)),
    )
    .chain(
        document
            .reactions
            .iter()
            .map(|(name, reaction)| ("reaction", name, &reaction.argv)),
    );
    for (kind, name, argv) in declared {
        if !valid_action_name(name) {
            return Err(Invalid::ActionName);
        }
        check_argv(argv, &executable).map_err(|fault| Invalid::Argv {
            kind,
            action: name.clone(),
            fault,
        })?;
    }
    if let Some((name, _job)) = document
        .jobs
        .iter()
        .find(|(_name, job)| job.every_s.get() > MAX_JOB_INTERVAL.as_secs())
    {
        return Err(Invalid::Interval { job: name.clone() });
    }
    for (name, reaction) in &document.reactions {
        if !events_valid(&reaction.events) {
            return Err(Invalid::Events {
                reaction: name.clone(),
            });
        }
    }
    Ok(Manifest {
        identity: PackageIdentity {
            id: document.plugin.id,
            version: document.plugin.version,
        },
        description,
        actions: document.actions,
        jobs: document.jobs,
        services: document.services,
        reactions: document.reactions,
    })
}

/// Whether `events` is non-empty, bounded, distinct and reactable.
fn events_valid(events: &[String]) -> bool {
    if events.is_empty() || events.len() > MAX_REACTION_EVENTS {
        return false;
    }
    let mut seen = std::collections::BTreeSet::new();
    events
        .iter()
        .all(|event| reactable(event) && seen.insert(event.as_str()))
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
