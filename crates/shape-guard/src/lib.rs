//! Shared machinery of the persisted-shape guard tests.
//!
//! A guard serializes fully populated samples of a persisted record, collects
//! the recursive key set and the string-valued leaves, classifies every leaf as
//! a registered enum value or free text, and compares the resulting lines with
//! a checked-in snapshot for the current schema or format constant. This crate
//! holds the parts the guards of the session store, the host state records, the
//! notification store and the package registry have in common, so that a fix
//! lands in all of them at once.
//!
//! The crate is a dev-dependency of the guard owners only and is never a normal
//! dependency of a shipped binary.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Debug, Display};

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

/// String-valued leaves of a serialized record: leaf path to the values seen there.
pub type Leaves = BTreeMap<String, BTreeSet<String>>;

/// One persisted enum: every leaf path it is stored at and its serialized values.
#[derive(Debug)]
pub struct EnumEntry {
    pub paths: Vec<String>,
    pub values: Vec<String>,
}

/// Records the key path of every object member, and every string leaf, below
/// `value`. Arrays contribute `[]` to the path of their items. The members of an
/// object whose path is in `dynamic_maps` are folded into one `*` key, so the
/// sample's map keys are not pinned.
fn walk(
    path: &str,
    value: &Value,
    dynamic_maps: &[&str],
    keys: &mut Option<&mut BTreeSet<String>>,
    leaves: &mut Option<&mut Leaves>,
) {
    match value {
        Value::Object(map) => {
            let folded = dynamic_maps.contains(&path);
            for (key, child) in map {
                let key = if folded { "*" } else { key.as_str() };
                let child_path = if path.is_empty() {
                    key.to_owned()
                } else {
                    format!("{path}.{key}")
                };
                if let Some(keys) = keys {
                    keys.insert(child_path.clone());
                }
                walk(&child_path, child, dynamic_maps, keys, leaves);
            }
        }
        Value::Array(items) => {
            let item_path = format!("{path}[]");
            for item in items {
                walk(&item_path, item, dynamic_maps, keys, leaves);
            }
        }
        Value::String(text) => {
            if let Some(leaves) = leaves {
                leaves
                    .entry(path.to_owned())
                    .or_default()
                    .insert(text.clone());
            }
        }
        _ => {}
    }
}

/// Adds the key path of every object member below `value` to `out`.
pub fn collect_paths(prefix: &str, value: &Value, out: &mut BTreeSet<String>) {
    walk(prefix, value, &[], &mut Some(out), &mut None);
}

/// Adds every string leaf below `value` to `out`.
pub fn collect_string_leaves(prefix: &str, value: &Value, out: &mut Leaves) {
    walk(prefix, value, &[], &mut None, &mut Some(out));
}

/// Adds the key paths and the string leaves below `value`, folding the members
/// of each map named in `dynamic_maps` into `<map>.*`.
pub fn collect(
    prefix: &str,
    value: &Value,
    dynamic_maps: &[&str],
    keys: &mut BTreeSet<String>,
    leaves: &mut Leaves,
) {
    walk(
        prefix,
        value,
        dynamic_maps,
        &mut Some(keys),
        &mut Some(leaves),
    );
}

/// Serialized values of `samples`, each checked to deserialize strictly back to
/// itself. A plain string is the value. A data-carrying variant contributes the
/// string under `tag` when the enum is internally tagged, or its only key when it
/// is externally tagged (`{"literal": ..}`), so every variant name is a value.
///
/// # Panics
///
/// When a sample does not round-trip or has no recognizable variant name.
pub fn string_values<T>(samples: &[T], tag: Option<&str>) -> Vec<String>
where
    T: Serialize + DeserializeOwned + Debug,
{
    let mut values = BTreeSet::new();
    for sample in samples {
        let json = serde_json::to_value(sample).expect("sample serializes");
        let back: T = serde_json::from_value(json.clone()).expect("sample deserializes strictly");
        assert_eq!(
            serde_json::to_value(&back).expect("round-tripped sample serializes"),
            json,
            "a listed value must round-trip"
        );
        let value = match (&json, tag) {
            (Value::String(text), None) => text.clone(),
            (Value::Object(map), Some(tag)) => map
                .get(tag)
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("{json} carries no string under `{tag}`"))
                .to_owned(),
            (Value::Object(map), None) if map.len() == 1 => map
                .keys()
                .next()
                .expect("an object with one key has a key")
                .clone(),
            _ => panic!("{json} is not a string, a tagged object or a one-key object"),
        };
        values.insert(value);
    }
    values.into_iter().collect()
}

/// Asserts that an `Unknown` variant never survives a strict round trip, so the
/// daemon cannot persist it and an older store never needs to read it.
///
/// # Panics
///
/// When `value` serializes and deserializes back to itself.
pub fn assert_never_round_trips<T>(value: &T)
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    let round_trips = serde_json::to_value(value)
        .ok()
        .and_then(|json| serde_json::from_value::<T>(json).ok())
        .is_some_and(|back| back == *value);
    assert!(
        !round_trips,
        "{value:?} must not round-trip through strict serde"
    );
}

/// Maps a map-valued leaf (`...metadata.<key>`) to its `...metadata.*` entry.
#[must_use]
pub fn normalized_leaf(path: &str) -> String {
    let segments: Vec<&str> = path.split('.').collect();
    match segments.iter().position(|segment| *segment == "metadata") {
        Some(index) if index + 1 < segments.len() => {
            format!("{}.*", segments[..=index].join("."))
        }
        _ => path.to_owned(),
    }
}

/// Checks that every string leaf is a registered enum value or free text, that
/// every path of every registry entry matches a leaf, and returns the
/// `path=value` snapshot lines of the registered enums.
///
/// # Errors
///
/// A message listing every unclassified leaf, foreign value and unmatched entry.
pub fn classify_leaves(
    leaves: &Leaves,
    enums: &[EnumEntry],
    free: &[Vec<String>],
) -> Result<BTreeSet<String>, String> {
    let mut problems = Vec::new();
    let mut seen = BTreeSet::new();
    for (path, values) in leaves {
        let key = normalized_leaf(path);
        seen.insert(key.clone());
        if let Some(entry) = enums.iter().find(|entry| entry.paths.contains(&key)) {
            for value in values.iter().filter(|value| !entry.values.contains(*value)) {
                problems.push(format!(
                    "{path}={value} is not a value of the enum registered for that path"
                ));
            }
        } else if !free.iter().any(|paths| paths.contains(&key)) {
            problems.push(format!(
                "string leaf {path} is neither a registered enum path nor free text"
            ));
        }
    }
    for entry in enums
        .iter()
        .filter(|entry| !entry.paths.iter().all(|path| seen.contains(path)))
    {
        problems.push(format!(
            "registered enum paths {:?} match no sample leaf",
            entry.paths
        ));
    }
    for paths in free
        .iter()
        .filter(|paths| !paths.iter().all(|path| seen.contains(path)))
    {
        problems.push(format!("free-text entry {paths:?} matches no sample leaf"));
    }
    if !problems.is_empty() {
        return Err(format!(
            "the persisted string leaves are not fully classified:\n  {}",
            problems.join("\n  ")
        ));
    }
    Ok(enums
        .iter()
        .flat_map(|entry| {
            entry.paths.iter().flat_map(|path| {
                entry
                    .values
                    .iter()
                    .map(move |value| format!("{path}={value}"))
            })
        })
        .collect())
}

/// What a snapshot comparison names in its failure message.
#[derive(Debug)]
pub struct SnapshotGuard<'a> {
    /// The guarded record, as in "the serialized {record} changed".
    pub record: &'a str,
    /// The kind of version constant: `schema` or `format`.
    pub bump: &'a str,
    /// The version constant, as spelled in the source.
    pub constant: &'a str,
    /// Repository path of a snapshot file with `<N>` standing for the version.
    pub fixture: &'a str,
    /// What the failing change must additionally come with, between the bump and
    /// the new snapshot: `make the reader accept the previous format as well`.
    pub follow_up: &'a str,
}

/// The snapshot lines as the file stores them.
#[must_use]
pub fn render(shape: &BTreeSet<String>) -> String {
    shape
        .iter()
        .flat_map(|line| [line.as_str(), "\n"])
        .collect()
}

/// Compares `actual` with the snapshot registered for `version`.
///
/// # Errors
///
/// A message with the added and removed lines and the content of the new
/// snapshot when the shape differs, or when `version` has no snapshot.
pub fn check_snapshot<V: Copy + Display + PartialEq>(
    guard: &SnapshotGuard<'_>,
    version: V,
    actual: &BTreeSet<String>,
    snapshots: &[(V, &str)],
) -> Result<(), String> {
    let rendered = render(actual);
    let SnapshotGuard {
        record,
        bump,
        constant,
        fixture,
        follow_up,
    } = guard;
    let Some((_, expected)) = snapshots.iter().find(|(known, _)| *known == version) else {
        let fixture = fixture.replace("<N>", &version.to_string());
        return Err(format!(
            "{record} {bump} {version} has no snapshot. Add {fixture} with the content below \
             and register it:\n{rendered}"
        ));
    };
    let expected: BTreeSet<String> = expected.lines().map(str::to_owned).collect();
    if &expected == actual {
        return Ok(());
    }
    let added: Vec<_> = actual.difference(&expected).collect();
    let removed: Vec<_> = expected.difference(actual).collect();
    Err(format!(
        "the serialized {record} changed without a {bump} bump ({constant} = {version}).\n  \
         added: {added:?}\n  removed: {removed:?}\n\
         Bump {constant}, {follow_up}, and add {fixture} with the content below:\n{rendered}"
    ))
}

/// Serialized values of a unit-variant enum. The variants after `;` are skipped:
/// they contribute no value and must not round-trip. The generated `match` has no
/// wildcard arm, so a variant missing from both lists fails to compile.
#[macro_export]
macro_rules! unit_values {
    ($ty:ident: $($variant:ident),+ $(; skip $($skipped:ident),+)?) => {{
        let listed = [$($ty::$variant),+];
        for value in &listed {
            match value {
                $($ty::$variant => {})+
                $($($ty::$skipped => {})+)?
            }
        }
        $($($crate::assert_never_round_trips(&$ty::$skipped);)+)?
        $crate::string_values(&listed, None)
    }};
}

/// Serialized values of an enum with data-carrying variants. Each entry pairs a
/// variant with the sample that must match it, and the generated `match` lists the
/// same variants without a wildcard arm: a new variant fails to compile until an
/// entry names it, and an entry cannot name a variant without supplying its sample.
///
/// An entry names exactly one variant, `Type::Variant`, followed by the tail that
/// ignores its fields, `(..)` or `{ .. }`. An or-pattern, a parenthesized
/// pattern or `_` does not parse, so one entry can never stand for several
/// variants. `$tag` is the internal tag key, or `None` for an externally tagged
/// enum.
///
/// ```
/// use persisted_shape::data_values;
///
/// #[derive(Debug, serde::Serialize, serde::Deserialize)]
/// #[serde(rename_all = "snake_case")]
/// enum Arg {
///     Literal(String),
///     Reference,
/// }
///
/// let values = data_values!(
///     Arg, None;
///     Arg::Literal(..) => Arg::Literal("x".to_owned()),
///     Arg::Reference => Arg::Reference,
/// );
/// assert_eq!(values, ["literal", "reference"]);
/// ```
///
/// A top-level or-pattern does not compile:
///
/// ```compile_fail
/// use persisted_shape::data_values;
///
/// #[derive(Debug, serde::Serialize, serde::Deserialize)]
/// enum Arg {
///     Literal(String),
///     Reference,
/// }
///
/// data_values!(
///     Arg, None;
///     Arg::Literal(..) | Arg::Reference => Arg::Reference,
/// );
/// ```
///
/// Neither does a parenthesized or-pattern:
///
/// ```compile_fail
/// use persisted_shape::data_values;
///
/// #[derive(Debug, serde::Serialize, serde::Deserialize)]
/// enum Arg {
///     Literal(String),
///     Reference,
/// }
///
/// data_values!(
///     Arg, None;
///     (Arg::Literal(..) | Arg::Reference) => Arg::Reference,
/// );
/// ```
///
/// Nor does a wildcard:
///
/// ```compile_fail
/// use persisted_shape::data_values;
///
/// #[derive(Debug, serde::Serialize, serde::Deserialize)]
/// enum Arg {
///     Literal(String),
///     Reference,
/// }
///
/// data_values!(
///     Arg, None;
///     _ => Arg::Reference,
/// );
/// ```
///
/// A variant that is not listed fails the exhaustive `match`:
///
/// ```compile_fail
/// use persisted_shape::data_values;
///
/// #[derive(Debug, serde::Serialize, serde::Deserialize)]
/// enum Arg {
///     Literal(String),
///     Reference,
///     Env(String),
/// }
///
/// data_values!(
///     Arg, None;
///     Arg::Literal(..) => Arg::Literal("x".to_owned()),
///     Arg::Reference => Arg::Reference,
/// );
/// ```
#[macro_export]
macro_rules! data_values {
    (
        $ty:ty, $tag:expr;
        $($entry_ty:ident :: $variant:ident
            $(($($tuple:tt)*))? $({$($record:tt)*})?
            => $sample:expr),+ $(,)?
    ) => {{
        let listed: ::std::vec::Vec<$ty> = ::std::vec![$($sample),+];
        let matchers: ::std::vec::Vec<fn(&$ty) -> bool> = ::std::vec![$(
            |value| matches!(
                value,
                $entry_ty::$variant $(($($tuple)*))? $({$($record)*})?
            )
        ),+];
        for (value, matcher) in listed.iter().zip(&matchers) {
            assert!(
                matcher(value),
                "{value:?} does not match the pattern it is listed under"
            );
        }
        for value in &listed {
            match value {
                $($entry_ty::$variant $(($($tuple)*))? $({$($record)*})? => {})+
            }
        }
        $crate::string_values(&listed, $tag)
    }};
}

#[cfg(test)]
mod tests;
