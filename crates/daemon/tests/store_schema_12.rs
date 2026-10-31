//! Schema-11 store migration through the public daemon store boundary.
//!
//! The fixture is a schema-11 record of a lost Pi session, in the shape the
//! schema-11 daemon writes, with neutral paths and identifiers. Its recovery
//! snapshot records no `refuse_while_blocked`, which reads `false`.

// Rust guideline compliant 2026-10-31

use pohunek_daemon::store::{SchemaMigration, Store, STORE_SCHEMA_VERSION};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;

const SCHEMA_11: &str = include_str!("../src/store/fixtures/schema-11/metadata.jsonl");

fn fixture_store(root: &std::path::Path) -> Store {
    let path = root.join("metadata.jsonl");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("create owner-private schema-11 store")
        .write_all(SCHEMA_11.as_bytes())
        .expect("write frozen schema-11 store");
    Store::new(path)
}

#[test]
fn schema_11_recovery_snapshot_migrates_without_inventing_a_refusal() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = fixture_store(root.path());

    let migrated = store.migrate_to_current().expect("migrate previous schema");
    let SchemaMigration::Migrated {
        from,
        to,
        records,
        backup,
    } = migrated
    else {
        panic!("schema-11 store must migrate");
    };
    assert_eq!((from, to, records), (11, STORE_SCHEMA_VERSION, 1));
    assert_eq!(
        std::fs::read_to_string(backup).expect("read backup"),
        SCHEMA_11
    );

    let sessions = store.load_sessions().expect("load migrated session");
    let recovery = sessions[0].recovery.as_ref().expect("recovery snapshot");
    assert!(
        !recovery.input_rules.refuse_while_blocked,
        "a schema 11 snapshot records no refusal"
    );
}

#[test]
fn a_recorded_refusal_round_trips_through_the_store() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = fixture_store(root.path());
    store.migrate_to_current().expect("migrate previous schema");
    let mut record = store
        .load_sessions()
        .expect("load migrated session")
        .remove(0);
    record
        .recovery
        .as_mut()
        .expect("recovery snapshot")
        .input_rules
        .refuse_while_blocked = true;
    store.record_session(&record).expect("record the refusal");

    let reloaded = store.load_sessions().expect("reload the session");
    let rules = reloaded[0].recovery.as_ref().expect("recovery").input_rules;
    assert!(rules.refuse_while_blocked);
    assert!(
        !rules.restricted,
        "the refusal is not the safe-text contract"
    );
}
