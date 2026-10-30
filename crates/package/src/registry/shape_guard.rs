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

use persisted_shape::{
    check_snapshot, classify_leaves, collect, unit_values, EnumEntry, Leaves, SnapshotGuard,
};
use protocol::{PackageDigest, PackageId, PackageIdentity, PackageVersion};
use serde_json::Value;

use super::{PackageRecord, PackageSource, RegistryState, REGISTRY_SCHEMA};
use crate::manifest::ManifestDigest;

/// Registry key and value snapshots by schema version.
const SNAPSHOTS: &[(u32, &str)] = &[(1, include_str!("fixtures/registry-1.txt"))];

/// Leaf paths that hold digests, identifiers and versions.
fn free_text() -> Vec<Vec<String>> {
    [
        "registry.packages[].digest",
        "registry.packages[].identity.id",
        "registry.packages[].identity.version",
        "registry.packages[].manifest",
        "registry.selected.*",
    ]
    .iter()
    .map(|path| vec![(*path).to_owned()])
    .collect()
}

/// Maps whose keys are data, not schema: folded into `<map>.*`.
const DYNAMIC_MAPS: &[&str] = &["registry.selected"];

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

fn enums() -> Vec<EnumEntry> {
    vec![EnumEntry {
        paths: vec!["registry.packages[].source".to_owned()],
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

fn registry_shape() -> BTreeSet<String> {
    let state = sample();
    assert_fields_are_reviewed(&state);
    let document: Value = serde_json::from_slice(&serde_json::to_vec(&state).expect("encode"))
        .expect("registry json");
    let mut shape = BTreeSet::from(["registry".to_owned()]);
    let mut leaves = Leaves::new();
    collect("registry", &document, DYNAMIC_MAPS, &mut shape, &mut leaves);
    shape.extend(
        classify_leaves(&leaves, &enums(), &free_text())
            .unwrap_or_else(|message| panic!("{message}")),
    );
    shape
}

/// Compares `actual` with the snapshot kept for `version`.
fn check_schema(
    version: u32,
    actual: &BTreeSet<String>,
    snapshots: &[(u32, &str)],
) -> Result<(), String> {
    check_snapshot(
        &SnapshotGuard {
            record: "registry record",
            bump: "schema",
            constant: "REGISTRY_SCHEMA",
            fixture: "crates/package/src/registry/fixtures/registry-<N>.txt",
            follow_up: "make parse_state accept the previous schema as well",
        },
        version,
        actual,
        snapshots,
    )
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
    let message = classify_leaves(&leaves, &enums(), &free_text())
        .expect_err("an unclassified leaf must fail");
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
    let message =
        classify_leaves(&leaves, &enums(), &free_text()).expect_err("a foreign value must fail");
    assert!(
        message.contains("registry.packages[].source=builtin is not a value"),
        "{message}"
    );
}
