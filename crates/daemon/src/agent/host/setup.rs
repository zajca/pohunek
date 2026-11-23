//! Package setup entries: the `[setup.<name>]` export of a runtime descriptor.
//!
//! A setup entry names an interactive program of the package that the owner
//! runs from a local terminal with `pohunek plugin run`. The daemon never runs
//! it: `package.setup_resolve` only returns the verified program path and the
//! fixed arguments. The entries are part of a runtime descriptor, behind the
//! `package-setup-v1` feature, so a setup-only package is refused.

// Rust guideline compliant 2026-11-23

use std::collections::BTreeMap;

use protocol::{CommandName, SetupInfo};
use serde::Deserialize;
use toml::Table;

use super::definition::{validate_token, DefinitionError, MAX_ARG_BYTES, MAX_LAUNCH_ARGS};

/// Maximum number of setup entries one runtime descriptor exports.
///
/// Every entry is listed in the install review the owner reads before
/// consenting, so the list stays short enough to read.
const MAX_PACKAGE_SETUP: usize = 8;

/// Longest description of a setup entry, in bytes.
///
/// The description is one line in the install review and in `plugin inspect`;
/// the bound keeps a review that lists every entry on one screen.
const MAX_SETUP_DESCRIPTION_BYTES: usize = 200;

/// One `[setup.<name>]` table.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSetup {
    program: String,
    #[serde(default)]
    args: Vec<String>,
    description: String,
}

/// Validates the `[setup.<name>]` tables of `descriptor`.
///
/// `executable` answers whether the package manifest lists a file at a path
/// and whether it is marked executable, with the same rule as a command's
/// `program`. A descriptor without a `setup` key yields no entries.
///
/// # Errors
///
/// [`DefinitionError::Field`] naming the violated rule, never the value.
pub(super) fn setup_of(
    descriptor: &Table,
    executable: impl Fn(&str) -> Option<bool>,
) -> Result<Vec<SetupInfo>, DefinitionError> {
    let Some(declared) = descriptor.get("setup") else {
        return Ok(Vec::new());
    };
    let field = |reason| DefinitionError::Field {
        field: "setup",
        reason,
    };
    let Some(table) = declared.as_table() else {
        return Err(field("must be a table of named entries"));
    };
    if table.is_empty() {
        return Err(field("declares no entry"));
    }
    if table.len() > MAX_PACKAGE_SETUP {
        return Err(field("has too many entries"));
    }
    let mut entries = BTreeMap::new();
    for (name, value) in table {
        let name = CommandName::parse(name).map_err(|_error| field("has an invalid entry name"))?;
        let raw: RawSetup = value
            .clone()
            .try_into()
            .map_err(|_error| field("has an entry that does not match the setup schema"))?;
        let entry = validated(name.clone(), raw, &executable)?;
        entries.insert(name, entry);
    }
    Ok(entries.into_values().collect())
}

fn validated(
    name: CommandName,
    raw: RawSetup,
    executable: &impl Fn(&str) -> Option<bool>,
) -> Result<SetupInfo, DefinitionError> {
    let field = |field, reason| DefinitionError::Field { field, reason };
    if raw.program.is_empty() || raw.program.len() > MAX_ARG_BYTES {
        return Err(field(
            "setup.program",
            "must be non-empty and within the argument length limit",
        ));
    }
    if executable(&raw.program) != Some(true) {
        return Err(field(
            "setup.program",
            "must name an executable file listed in the package",
        ));
    }
    if raw.args.len() > MAX_LAUNCH_ARGS {
        return Err(field("setup.args", "has too many arguments"));
    }
    for arg in &raw.args {
        validate_token(arg, "setup.args")?;
    }
    if raw.description.is_empty()
        || raw.description.len() > MAX_SETUP_DESCRIPTION_BYTES
        || raw.description.chars().any(char::is_control)
    {
        return Err(field(
            "setup.description",
            "must be 1..=200 bytes without control characters",
        ));
    }
    Ok(SetupInfo {
        name,
        program: raw.program,
        args: raw.args,
        description: raw.description,
    })
}
