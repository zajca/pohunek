//! Package commands: the `[commands.<name>]` export of a package descriptor.
//!
//! A command is a fixed argv the daemon runs as the owner when the command is
//! called: a package-relative `program`, literal `args` and an `expose`
//! scope. The descriptor is the only source of all three; a call carries no
//! argument, environment or input of its own. Commands are gated by the
//! `package-commands-v1` runtime feature, so a core that lacks the feature
//! refuses the descriptor at install.
//!
//! A descriptor exports a runtime (`[runtime]`, `[input]`, `[resume]`,
//! `[fork]`, `[native_reference]`, all or none), commands, or both, and at
//! least one of the two. A package that exports commands only serves no
//! runtime: it is never launchable as a session.

// Rust guideline compliant 2026-10-28

use std::collections::BTreeMap;

use protocol::{
    CommandExposure, CommandInfo, CommandName, PackageId, PackageIdentity, PackageVersion,
};
use serde::Deserialize;
use toml::{Table, Value};

use super::definition::{
    malformed_at, validate_token, DefinitionError, RuntimeDefinition, MAX_ARG_BYTES,
    MAX_LAUNCH_ARGS, SUPPORTED_RUNTIME_API, SUPPORTED_SCHEMA,
};
use super::features::{check_gates, check_requires, FeatureTables};

/// Maximum number of commands one package exports.
///
/// Every command is listed in the install review the owner reads before
/// consenting, so the list stays short enough to read.
pub const MAX_PACKAGE_COMMANDS: usize = 16;

/// Top-level tables that make up the runtime export, declared together or not
/// at all.
const RUNTIME_GROUP: [&str; 5] = ["runtime", "input", "resume", "fork", "native_reference"];

/// What a package descriptor exports.
#[derive(Debug, Clone)]
pub struct PackageDescriptor {
    identity: PackageIdentity,
    runtime: Option<RuntimeDefinition>,
    commands: Vec<CommandInfo>,
}

impl PackageDescriptor {
    /// The package id and version the descriptor declares.
    #[must_use]
    pub fn identity(&self) -> &PackageIdentity {
        &self.identity
    }

    /// The runtime the package serves; `None` for a package that exports
    /// commands only.
    #[must_use]
    pub fn runtime(&self) -> Option<&RuntimeDefinition> {
        self.runtime.as_ref()
    }

    /// Consumes the descriptor and returns its runtime.
    #[must_use]
    pub fn into_runtime(self) -> Option<RuntimeDefinition> {
        self.runtime
    }

    /// The commands the package exports, in name order.
    #[must_use]
    pub fn commands(&self) -> &[CommandInfo] {
        &self.commands
    }

    /// Splits the descriptor into its runtime and commands.
    #[must_use]
    pub fn into_parts(self) -> (Option<RuntimeDefinition>, Vec<CommandInfo>) {
        (self.runtime, self.commands)
    }

    /// A descriptor that exports `runtime` and `commands`.
    #[must_use]
    pub(super) fn with_commands(
        identity: PackageIdentity,
        runtime: RuntimeDefinition,
        commands: Vec<CommandInfo>,
    ) -> Self {
        Self {
            identity,
            runtime: Some(runtime),
            commands,
        }
    }
}

/// Which exports a descriptor declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Exports {
    /// The runtime group, with or without commands.
    Runtime,
    /// Commands only.
    CommandsOnly,
}

/// The keys of a command-only descriptor.
///
/// Anything else, including the optional runtime tables (`[integration]`,
/// `[launch_env]`, `[naming]`, `[config_home]`, `[transcripts]`), is refused: those belong
/// to a runtime export.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommandsDocument {
    schema: u32,
    id: String,
    version: String,
    runtime_api: u32,
    /// Checked on the untyped document before this struct is parsed.
    #[serde(default, rename = "requires")]
    _requires: Vec<String>,
    /// Validated from the untyped document, next to the manifest.
    #[serde(rename = "commands")]
    _commands: Value,
}

/// One `[commands.<name>]` table.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommand {
    program: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    expose: RawExpose,
}

/// `expose` as written in a descriptor; absent means local.
#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawExpose {
    #[default]
    Local,
    Peers,
}

impl RawExpose {
    fn into_exposure(self) -> CommandExposure {
        match self {
            Self::Local => CommandExposure::Local,
            Self::Peers => CommandExposure::Peers,
        }
    }
}

/// Classifies `descriptor` by the exports it declares, after the `requires`
/// check that must win over any shape error.
///
/// `requires` is checked first so an unknown feature wins over the parse error
/// of a key only that feature's core understands. The feature gates run later,
/// after the schema and runtime API checks, in the parser of each export.
///
/// # Errors
///
/// The `requires` errors of [`check_requires`], and [`DefinitionError::Field`]
/// for a runtime group that is only partly declared or a descriptor that
/// exports neither a runtime nor commands.
pub(super) fn classify(
    descriptor: &Table,
    features: &FeatureTables<'_>,
) -> Result<Exports, DefinitionError> {
    check_requires(descriptor, features)?;
    let declared = RUNTIME_GROUP
        .iter()
        .filter(|key| descriptor.contains_key(**key))
        .count();
    if declared == RUNTIME_GROUP.len() {
        return Ok(Exports::Runtime);
    }
    if declared != 0 {
        return Err(DefinitionError::Field {
            field: "runtime",
            reason: "runtime, input, resume, fork and native_reference are declared together or not at all",
        });
    }
    if descriptor.contains_key("commands") {
        Ok(Exports::CommandsOnly)
    } else {
        Err(DefinitionError::Field {
            field: "runtime",
            reason: "the descriptor exports neither a runtime nor commands",
        })
    }
}

/// Parses a command-only descriptor into its identity and commands.
///
/// The checks run in the order a runtime descriptor meets them: the document
/// shape, the schema and runtime API, the feature gates, then the commands.
///
/// # Errors
///
/// [`DefinitionError::Malformed`] when the document does not match the
/// command-only schema, the schema and runtime API errors of a runtime
/// descriptor, the gate errors of [`check_gates`], and the errors of
/// [`commands_of`].
pub(super) fn parse_commands_only(
    source: &str,
    descriptor: &Table,
    features: &FeatureTables<'_>,
    executable: impl Fn(&str) -> Option<bool>,
) -> Result<PackageDescriptor, DefinitionError> {
    let raw: RawCommandsDocument =
        toml::from_str(source).map_err(|error| malformed_at(source, &error))?;
    if raw.schema != SUPPORTED_SCHEMA {
        return Err(DefinitionError::UnsupportedSchema { found: raw.schema });
    }
    if raw.runtime_api != SUPPORTED_RUNTIME_API {
        return Err(DefinitionError::UnsupportedRuntimeApi {
            found: raw.runtime_api,
        });
    }
    check_gates(descriptor, features)?;
    let identity = PackageIdentity {
        id: PackageId::parse(&raw.id).map_err(|source| DefinitionError::Identifier {
            field: "id",
            source,
        })?,
        version: PackageVersion::parse(&raw.version).map_err(|source| {
            DefinitionError::Binding {
                field: "version",
                source,
            }
        })?,
    };
    let commands = commands_of(descriptor, executable)?;
    Ok(PackageDescriptor {
        identity,
        runtime: None,
        commands,
    })
}

/// Validates the `[commands.<name>]` tables of `descriptor`.
///
/// `executable` answers whether the package manifest lists a file at a path
/// and whether it is marked executable. A `program` is accepted only when it
/// equals a manifest path exactly, so it is package-relative and normalized
/// (a leading `/` or a `..` segment matches no manifest entry), and only when
/// that entry carries the executable bit.
///
/// A descriptor without a `commands` key yields no commands.
///
/// # Errors
///
/// [`DefinitionError::Field`] naming the violated rule, never the value.
pub(super) fn commands_of(
    descriptor: &Table,
    executable: impl Fn(&str) -> Option<bool>,
) -> Result<Vec<CommandInfo>, DefinitionError> {
    let Some(declared) = descriptor.get("commands") else {
        return Ok(Vec::new());
    };
    let field = |reason| DefinitionError::Field {
        field: "commands",
        reason,
    };
    let Some(table) = declared.as_table() else {
        return Err(field("must be a table of named commands"));
    };
    if table.is_empty() {
        return Err(field("declares no command"));
    }
    if table.len() > MAX_PACKAGE_COMMANDS {
        return Err(field("has too many commands"));
    }
    let mut commands = BTreeMap::new();
    for (name, value) in table {
        let name =
            CommandName::parse(name).map_err(|_error| field("has an invalid command name"))?;
        let raw: RawCommand = value
            .clone()
            .try_into()
            .map_err(|_error| field("has a command that does not match the command schema"))?;
        let command = validated(name.clone(), raw, &executable)?;
        commands.insert(name, command);
    }
    Ok(commands.into_values().collect())
}

fn validated(
    name: CommandName,
    raw: RawCommand,
    executable: &impl Fn(&str) -> Option<bool>,
) -> Result<CommandInfo, DefinitionError> {
    let field = |field, reason| DefinitionError::Field { field, reason };
    if raw.program.is_empty() || raw.program.len() > MAX_ARG_BYTES {
        return Err(field(
            "commands.program",
            "must be non-empty and within the argument length limit",
        ));
    }
    if executable(&raw.program) != Some(true) {
        return Err(field(
            "commands.program",
            "must name an executable file listed in the package",
        ));
    }
    if raw.args.len() > MAX_LAUNCH_ARGS {
        return Err(field("commands.args", "has too many arguments"));
    }
    for arg in &raw.args {
        validate_token(arg, "commands.args")?;
    }
    Ok(CommandInfo {
        name,
        program: raw.program,
        args: raw.args,
        expose: raw.expose.into_exposure(),
    })
}
