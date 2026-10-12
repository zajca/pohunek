//! Schema-5 store migration through the public daemon store boundary.

// Rust guideline compliant 2026-10-12

use pohunek_daemon::agent::SessionRef;
use pohunek_daemon::store::{SchemaMigration, Store, STORE_SCHEMA_VERSION};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;

/// A schema-5 resume binding of a Pi package session: its frozen spec assigns
/// the launch reference and declares no fork assignment.
const SCHEMA_5: &str = include_str!("../src/store/fixtures/schema-5/metadata.jsonl");

#[test]
fn a_schema_5_assigned_binding_migrates_and_still_forks_without_a_reference_of_its_own() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let path = root.path().join("metadata.jsonl");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("create owner-private schema-5 store")
        .write_all(SCHEMA_5.as_bytes())
        .expect("write frozen schema-5 store");
    let store = Store::new(path.clone());

    let migrated = store.migrate_to_current().expect("migrate previous schema");
    let SchemaMigration::Migrated {
        from, to, records, ..
    } = migrated
    else {
        panic!("schema-5 store must migrate");
    };
    assert_eq!((from, to, records), (5, STORE_SCHEMA_VERSION, 1));
    let stored = std::fs::read_to_string(&path).expect("read migrated store");
    let line: serde_json::Value = serde_json::from_str(stored.trim()).expect("stored JSON");
    assert_eq!(line["schema_version"], STORE_SCHEMA_VERSION);
    assert!(
        line["native_launch"]["assigned"]
            .get("assign_fork")
            .is_none(),
        "the migration grants no fork assignment"
    );

    let bindings = store.load_resume().expect("load migrated binding");
    assert_eq!(bindings.len(), 1);
    let launch = bindings[0]
        .native_launch
        .as_ref()
        .expect("frozen native launch");
    let assigned = launch.assigned().expect("the launch reference is assigned");
    assert!(!assigned.assigns_fork());
    let source = SessionRef::id("0197aaaa-bbbb-7ccc-8ddd-eeeeeeeeeeee").expect("reference");
    let fork = launch.fork_argv(&source).expect("the session still forks");
    assert_eq!(fork.argv, ["--fork", source.value()]);
    assert_eq!(fork.reference, None, "its fork holds no assigned reference");
}
