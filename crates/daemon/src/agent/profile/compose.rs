//! Composes the text of a new host agent profile.
//!
//! The composer emits a fixed key order (`base`, `package`, `digest`, `args`,
//! `secret_env`, then the `[env]` table) so the same spec always yields the
//! same bytes and its digest is stable. The spec carries no free-form `[env]`
//! map: the only environment entry a profile composed here can hold is the
//! runtime's config home variable.

// Rust guideline compliant 2026-11-11

use protocol::RuntimeId;
use toml_edit::{Array, DocumentMut, Item, Table, Value};

use super::bind::Pin;

/// What a new profile holds.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    if let Some((name, path)) = &spec.home {
        let mut env = Table::new();
        env.insert(name, Item::Value(Value::from(path.as_str())));
        root.insert("env", Item::Table(env));
    }
    document.to_string()
}

fn strings(items: &[String]) -> Array {
    items.iter().map(String::as_str).collect()
}
