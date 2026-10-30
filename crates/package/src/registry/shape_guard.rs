//! Guard against registry record changes that skip a schema bump.
//!
//! The recursive key set and the serialized enum value set of a fully populated
//! `registry.json` are compared with the checked-in snapshot
//! `fixtures/registry-<N>.txt` for the current [`REGISTRY_SCHEMA`]. The reader
//! accepts exactly one schema, so a daemon that meets a record with a new key or a
//! new [`PackageSource`] value reports it as damaged. Every string leaf must be a
//! registered enum path or free text, so a new string-valued field cannot slip in
//! unclassified.
//!
//! When a snapshot no longer matches, the test fails. To resolve it:
//!
//! 1. bump [`REGISTRY_SCHEMA`],
//! 2. make `parse_state` accept the previous schema as well,
//! 3. add the new `fixtures/registry-<new>.txt` with the content the failure prints
//!    and register it in [`SNAPSHOTS`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;

use protocol::{PackageDigest, PackageId, PackageIdentity, PackageVersion};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;

use super::{PackageRecord, PackageSource, RegistryState, REGISTRY_SCHEMA};
use crate::manifest::ManifestDigest;

/// Registry key and value snapshots by schema version.
const SNAPSHOTS: &[(u32, &str)] = &[(1, include_str!("fixtures/registry-1.txt"))];

/// Leaf path to the string values serialized there.
type Leaves = BTreeMap<String, BTreeSet<String>>;

/// An enum-valued leaf path and the values the enum serializes to.
struct EnumEntry {
    path: &'static str,
    values: Vec<String>,
}

/// Leaf paths that hold digests, identifiers and versions.
const FREE_TEXT: &[&str] = &[
    "registry.packages[].digest",
    "registry.packages[].identity.id",
    "registry.packages[].identity.version",
    "registry.packages[].manifest",
    "registry.selected.*",
];

fn digest(byte: char) -> PackageDigest {
    PackageDigest::parse(&format!("sha256:{}", byte.to_string().repeat(64))).expect("digest")
}

fn record(byte: char, id: &str, source: PackageSource) -> PackageRecord {
    PackageRecord {
        digest: digest(byte),
        identity: PackageIdentity {
            id: PackageId::parse(id).expect("package id"),
            version: PackageVersion::parse("1.0.0").expect("package version"),
        },
        source,
        manifest: ManifestDigest::parse(&byte.to_string().repeat(64)).expect("manifest digest"),
        installed_at_unix_seconds: 1,
        enabled: true,
    }
}

/// Serialized values of the listed unit variants of a persisted enum. The list also
/// feeds a `match` without a wildcard arm, so a variant missing from it fails to
/// compile, and a listed value must round-trip through strict serde.
macro_rules! unit_values {
    ($ty:ident: $($variant:ident),+) => {{
        let listed = [$($ty::$variant),+];
        for value in &listed {
            match value {
                $($ty::$variant => {})+
            }
        }
        string_values(&listed)
    }};
}

fn string_values<T>(listed: &[T]) -> Vec<String>
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    let values: BTreeSet<String> = listed
        .iter()
        .map(|value| {
            let json = serde_json::to_value(value).expect("value serializes");
            let back: T = serde_json::from_value(json.clone()).expect("value deserializes");
            assert_eq!(&back, value, "a listed value must round-trip");
            json.as_str()
                .expect("a unit variant is a string")
                .to_owned()
        })
        .collect();
    values.into_iter().collect()
}

fn enums() -> Vec<EnumEntry> {
    vec![EnumEntry {
        path: "registry.packages[].source",
        values: unit_values!(PackageSource: Official, ExplicitDigest, Link),
    }]
}

/// A record that names one package per source and selects the first.
fn sample() -> RegistryState {
    let packages = vec![
        record('a', "acme.official", PackageSource::Official),
        record('b', "acme.pinned", PackageSource::ExplicitDigest),
        record('c', "acme.linked", PackageSource::Link),
    ];
    let selected = BTreeMap::from([(packages[0].identity.id.clone(), packages[0].digest.clone())]);
    let state = RegistryState {
        schema: REGISTRY_SCHEMA,
        generation: 3,
        packages,
        selected,
    };
    assert!(state.validate(), "the sample must be a valid record");
    state
}

/// Fails to compile when a record field is added or an enum field is retyped.
#[expect(
    clippy::unneeded_field_pattern,
    reason = "naming every field with `_` is what makes a field change a compile error"
)]
fn assert_fields_are_reviewed(state: &RegistryState) {
    let RegistryState {
        schema: _,
        generation: _,
        packages,
        selected: _,
    } = state;
    let PackageRecord {
        digest: _,
        identity: _,
        source,
        manifest: _,
        installed_at_unix_seconds: _,
        enabled: _,
    } = &packages[0];
    let _: &PackageSource = source;
}

/// Keys and string leaves of `value`, with the dynamic `selected` map keys folded
/// into `selected.*`.
fn collect(path: &str, value: &Value, keys: &mut BTreeSet<String>, leaves: &mut Leaves) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let child_path = if path == "registry.selected" {
                    "registry.selected.*".to_owned()
                } else {
                    format!("{path}.{key}")
                };
                keys.insert(child_path.clone());
                collect(&child_path, child, keys, leaves);
            }
        }
        Value::Array(items) => {
            let child_path = format!("{path}[]");
            for item in items {
                collect(&child_path, item, keys, leaves);
            }
        }
        Value::String(text) => {
            leaves
                .entry(path.to_owned())
                .or_default()
                .insert(text.clone());
        }
        _ => {}
    }
}

/// Checks every string leaf is a registered enum value or free text and returns
/// the `path=value` snapshot lines of the registered enums.
fn classify(leaves: &Leaves, enums: &[EnumEntry]) -> Result<BTreeSet<String>, String> {
    let mut problems = Vec::new();
    for (path, values) in leaves {
        if let Some(entry) = enums.iter().find(|entry| entry.path == path) {
            for value in values.iter().filter(|value| !entry.values.contains(*value)) {
                problems.push(format!(
                    "{path}={value} is not a value of the enum registered for that path"
                ));
            }
        } else if !FREE_TEXT.contains(&path.as_str()) {
            problems.push(format!(
                "string leaf {path} is neither a registered enum path nor free text"
            ));
        }
    }
    for entry in enums
        .iter()
        .filter(|entry| !leaves.contains_key(entry.path))
    {
        problems.push(format!(
            "registered enum path {} matches no sample leaf",
            entry.path
        ));
    }
    for path in FREE_TEXT.iter().filter(|path| !leaves.contains_key(**path)) {
        problems.push(format!("free-text entry {path} matches no sample leaf"));
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
            entry
                .values
                .iter()
                .map(move |value| format!("{}={value}", entry.path))
        })
        .collect())
}

fn registry_shape() -> BTreeSet<String> {
    let state = sample();
    assert_fields_are_reviewed(&state);
    let document: Value = serde_json::from_slice(&serde_json::to_vec(&state).expect("encode"))
        .expect("registry json");
    let mut shape = BTreeSet::from(["registry".to_owned()]);
    let mut leaves = Leaves::new();
    collect("registry", &document, &mut shape, &mut leaves);
    shape.extend(classify(&leaves, &enums()).unwrap_or_else(|message| panic!("{message}")));
    shape
}

/// Compares `actual` with the snapshot kept for `version`.
fn check_schema(
    version: u32,
    actual: &BTreeSet<String>,
    snapshots: &[(u32, &str)],
) -> Result<(), String> {
    let rendered: String = actual
        .iter()
        .flat_map(|line| [line.as_str(), "\n"])
        .collect();
    let Some((_, expected)) = snapshots.iter().find(|(known, _)| *known == version) else {
        return Err(format!(
            "registry schema {version} has no snapshot. Add \
             crates/package/src/registry/fixtures/registry-{version}.txt with the content \
             below and register it:\n{rendered}"
        ));
    };
    let expected: BTreeSet<String> = expected.lines().map(str::to_owned).collect();
    if &expected == actual {
        return Ok(());
    }
    let added: Vec<_> = actual.difference(&expected).collect();
    let removed: Vec<_> = expected.difference(actual).collect();
    Err(format!(
        "the serialized registry record changed without a schema bump \
         (REGISTRY_SCHEMA = {version}).\n  added: {added:?}\n  removed: {removed:?}\n\
         Bump REGISTRY_SCHEMA, make parse_state accept the previous schema as well, and add \
         fixtures/registry-<new>.txt with the content below:\n{rendered}"
    ))
}

#[test]
fn the_registry_record_matches_the_snapshot_of_the_current_schema() {
    if let Err(message) = check_schema(REGISTRY_SCHEMA, &registry_shape(), SNAPSHOTS) {
        panic!("{message}");
    }
}

#[test]
fn a_changed_registry_value_set_is_reported_as_an_unbumped_change() {
    let mut shape = registry_shape();
    shape.insert("registry.packages[].source=builtin".to_owned());
    let message = check_schema(REGISTRY_SCHEMA, &shape, SNAPSHOTS)
        .expect_err("an added source must fail the guard");
    assert!(message.contains("without a schema bump"), "{message}");
    assert!(message.contains("accept the previous schema"), "{message}");
    assert!(
        message.contains("registry.packages[].source=builtin"),
        "{message}"
    );

    let mut shape = registry_shape();
    shape.remove("registry.packages[].source=link");
    check_schema(REGISTRY_SCHEMA, &shape, SNAPSHOTS)
        .expect_err("a removed or renamed source must fail the guard");

    let mut shape = registry_shape();
    shape.insert("registry.new_key".to_owned());
    check_schema(REGISTRY_SCHEMA, &shape, SNAPSHOTS).expect_err("an added key must fail the guard");
}

#[test]
fn a_bumped_schema_without_a_snapshot_is_reported() {
    let message = check_schema(REGISTRY_SCHEMA + 1, &registry_shape(), SNAPSHOTS)
        .expect_err("a new schema needs its own snapshot");
    assert!(message.contains("has no snapshot"), "{message}");
}

#[test]
fn an_unclassified_string_leaf_fails_the_guard() {
    let mut leaves = Leaves::new();
    leaves.insert(
        "registry.packages[].channel".to_owned(),
        BTreeSet::from(["stable".to_owned()]),
    );
    let message = classify(&leaves, &enums()).expect_err("an unclassified leaf must fail");
    assert!(
        message.contains("registry.packages[].channel is neither"),
        "{message}"
    );
}

#[test]
fn a_sample_value_outside_its_enum_fails_the_guard() {
    let mut leaves = Leaves::new();
    leaves.insert(
        "registry.packages[].source".to_owned(),
        BTreeSet::from(["builtin".to_owned()]),
    );
    let message = classify(&leaves, &enums()).expect_err("a foreign value must fail");
    assert!(
        message.contains("registry.packages[].source=builtin is not a value"),
        "{message}"
    );
}
