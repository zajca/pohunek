//! Composes the text of a new host agent profile or preset.
//!
//! The composer emits a fixed key order (`base`, `package`, `digest`, `args`,
//! `secret_env`, then the `[options]` and `[env]` tables) so the same spec
//! always yields the same bytes and its digest is stable. The spec carries no
//! free-form `[env]` map: the only environment entry a profile composed here
//! can hold is the runtime's config home variable. A preset holds only its
//! `[preset]` table.

// Rust guideline compliant 2026-11-14

use std::collections::BTreeMap;

use protocol::RuntimeId;
use toml_edit::{Array, DocumentMut, Item, Table, Value};

use super::bind::Pin;

/// What a new profile holds.
#[derive(Debug, Clone)]
pub(crate) struct ProfileSpec {
    /// Base runtime the profile extends.
    pub(crate) base: RuntimeId,
    /// Package pin; absent for a built-in base.
    pub(crate) pin: Option<Pin>,
    /// The config home variable name and its absolute path value.
    pub(crate) home: Option<(String, String)>,
    /// Launch arguments appended to the program.
    pub(crate) args: Vec<String>,
    /// Names of the secret variables a launch fills.
    pub(crate) secret_env: Vec<String>,
    /// The `[options]` table, as the caller's options text holds it.
    pub(crate) options: Option<Table>,
}

/// What a new preset holds.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PresetSpec<'a> {
    /// The profile the preset launches.
    pub(crate) parent: &'a str,
    /// Choice id by option id.
    pub(crate) options: &'a BTreeMap<String, String>,
    /// Whether a launch of the preset refuses an option selection.
    pub(crate) locked: bool,
}

/// The profile text for `spec`.
pub(crate) fn compose_profile(spec: &ProfileSpec) -> String {
    let mut document = DocumentMut::new();
    let root = document.as_table_mut();
    root.insert("base", Item::Value(Value::from(spec.base.as_str())));
    if let Some(pin) = &spec.pin {
        root.insert("package", Item::Value(Value::from(pin.package.as_str())));
        root.insert("digest", Item::Value(Value::from(pin.digest.as_str())));
    }
    if !spec.args.is_empty() {
        root.insert("args", Item::Value(Value::Array(strings(&spec.args))));
    }
    if !spec.secret_env.is_empty() {
        root.insert(
            "secret_env",
            Item::Value(Value::Array(strings(&spec.secret_env))),
        );
    }
    if let Some(options) = &spec.options {
        root.insert("options", Item::Table(options.clone()));
    }
    if let Some((name, path)) = &spec.home {
        let mut env = Table::new();
        env.insert(name, Item::Value(Value::from(path.as_str())));
        root.insert("env", Item::Table(env));
    }
    document.to_string()
}

/// The preset text for `spec`: `[preset]` with `parent`, `locked` when set,
/// and `[preset.options]` when it selects any choice.
pub(crate) fn compose_preset(spec: &PresetSpec<'_>) -> String {
    let mut document = DocumentMut::new();
    let mut preset = Table::new();
    preset.insert("parent", Item::Value(Value::from(spec.parent)));
    if spec.locked {
        preset.insert("locked", Item::Value(Value::from(true)));
    }
    if !spec.options.is_empty() {
        let mut options = Table::new();
        for (option, choice) in spec.options {
            options.insert(option, Item::Value(Value::from(choice.as_str())));
        }
        preset.insert("options", Item::Table(options));
    }
    document
        .as_table_mut()
        .insert("preset", Item::Table(preset));
    document.to_string()
}

fn strings(items: &[String]) -> Array {
    items.iter().map(String::as_str).collect()
}
