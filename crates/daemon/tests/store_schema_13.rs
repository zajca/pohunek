//! Schema-12 store migration through the public daemon store boundary, and the
//! spawner record of the current schema.
//!
//! The fixture is a schema-12 record of a lost Pi session, in the shape the
//! schema-12 daemon writes, with neutral paths and identifiers. A session that
//! records no spawner reads as none.

// Rust guideline compliant 2026-11-01

use pohunek_daemon::store::{SchemaMigration, Store, STORE_SCHEMA_VERSION};
use protocol::SpawnedBy;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;

const SCHEMA_12: &str = include_str!("../src/store/fixtures/schema-12/metadata.jsonl");

fn fixture_store(root: &std::path::Path) -> Store {
    let path = root.join("metadata.jsonl");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("create owner-private schema-12 store")
        .write_all(SCHEMA_12.as_bytes())
        .expect("write frozen schema-12 store");
    Store::new(path)
}

#[test]
fn schema_12_sessions_migrate_without_inventing_a_spawner() {
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
        panic!("schema-12 store must migrate");
    };
    assert_eq!((from, to, records), (12, STORE_SCHEMA_VERSION, 1));
    assert_eq!(
        std::fs::read_to_string(backup).expect("read backup"),
        SCHEMA_12
    );

    let sessions = store.load_sessions().expect("load migrated session");
    assert_eq!(sessions[0].info.spawned_by, None);
    assert_eq!(
        sessions[0].recovery.as_ref().expect("recovery").spawned_by,
        None
    );
}

#[test]
fn a_recorded_spawner_round_trips_through_the_store() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = fixture_store(root.path());
    store.migrate_to_current().expect("migrate previous schema");
    let spawner = SpawnedBy {
        session_id: protocol::SessionId("s-parent".to_owned()),
        daemon_id: "d-parent".to_owned(),
    };
    let mut record = store
        .load_sessions()
        .expect("load migrated session")
        .remove(0);
    record.info.spawned_by = Some(spawner.clone());
    record.recovery.as_mut().expect("recovery").spawned_by = Some(spawner.clone());
    store.record_session(&record).expect("record the spawner");

    let reloaded = store.load_sessions().expect("reload the session");
    assert_eq!(reloaded[0].info.spawned_by.as_ref(), Some(&spawner));
    assert_eq!(
        reloaded[0]
            .recovery
            .as_ref()
            .expect("recovery")
            .spawned_by
            .as_ref(),
        Some(&spawner)
    );
}
