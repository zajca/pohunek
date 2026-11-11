//! Schema-15 store migration through the public daemon store boundary, and the
//! launch checkout of a session in the current schema.
//!
//! The schema-15 fixture is a lost Pi session in the shape the schema-15 daemon
//! writes: its binding records no launch checkout, which the current schema
//! reads as none and never backfills from the session's directory. The
//! current-schema fixture records the directory and project the session was created
//! in, and both survive a rewrite of the store.

// Rust guideline compliant 2026-11-06

use pohunek_daemon::store::{LaunchCheckout, SchemaMigration, Store, STORE_SCHEMA_VERSION};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::PathBuf;

const SCHEMA_15: &str = include_str!("../src/store/fixtures/schema-15/metadata.jsonl");
const SCHEMA_18: &str = include_str!("../src/store/fixtures/schema-18/metadata.jsonl");

fn fixture_store(root: &std::path::Path, contents: &str) -> Store {
    let path = root.join("metadata.jsonl");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("create owner-private store")
        .write_all(contents.as_bytes())
        .expect("write frozen store");
    Store::new(path)
}

#[test]
fn schema_15_sessions_migrate_without_inventing_a_launch_checkout() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = fixture_store(root.path(), SCHEMA_15);

    let migrated = store.migrate_to_current().expect("migrate previous schema");
    let SchemaMigration::Migrated {
        from,
        to,
        records,
        backup,
    } = migrated
    else {
        panic!("schema-15 store must migrate");
    };
    assert_eq!((from, to, records), (15, STORE_SCHEMA_VERSION, 2));
    assert_eq!(
        std::fs::read_to_string(backup).expect("read backup"),
        SCHEMA_15
    );

    for session in store.load_sessions().expect("load migrated sessions") {
        let recovery = session.recovery.expect("a recovery binding");
        assert_eq!(recovery.launch_checkout, None);
    }
}

#[test]
fn a_recorded_launch_checkout_loads_and_survives_a_rewrite() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = fixture_store(root.path(), SCHEMA_18);
    let expected = Some(Box::new(LaunchCheckout {
        cwd: PathBuf::from("/fixture"),
        project_id: Some("p-fixture".to_owned()),
    }));

    let mut record = store.load_sessions().expect("load the session").remove(0);
    assert_eq!(
        record.recovery.as_ref().expect("recovery").launch_checkout,
        expected
    );
    record.info.name = Some("renamed".to_owned());
    store.record_session(&record).expect("rewrite the record");
    let reloaded = store.load_sessions().expect("reload the session");
    assert_eq!(
        reloaded[0]
            .recovery
            .as_ref()
            .expect("recovery")
            .launch_checkout,
        expected
    );
}
