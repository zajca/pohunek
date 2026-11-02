//! Schema-14 store migration through the public daemon store boundary, and the
//! loss evidence of a runtime in the current schema.
//!
//! The schema-14 fixture is a lost Pi session in the shape the schema-14 daemon
//! writes, with neutral paths and identifiers. Its runtime records no loss
//! evidence, which the current schema reads as none; the evidence the current
//! schema adds survives a rewrite of the record.

// Rust guideline compliant 2026-11-02

use pohunek_daemon::store::{SchemaMigration, Store, STORE_SCHEMA_VERSION};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;

const SCHEMA_14: &str = include_str!("../src/store/fixtures/schema-14/metadata.jsonl");

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
fn schema_14_runtimes_migrate_without_inventing_loss_evidence() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = fixture_store(root.path(), SCHEMA_14);

    let migrated = store.migrate_to_current().expect("migrate previous schema");
    let SchemaMigration::Migrated {
        from,
        to,
        records,
        backup,
    } = migrated
    else {
        panic!("schema-14 store must migrate");
    };
    assert_eq!((from, to, records), (14, STORE_SCHEMA_VERSION, 1));
    assert_eq!(
        std::fs::read_to_string(backup).expect("read backup"),
        SCHEMA_14
    );

    let sessions = store.load_sessions().expect("load migrated session");
    let runtime = sessions[0].info.runtime.as_ref().expect("runtime");
    assert_eq!(runtime.loss_cause, None);
    assert_eq!(runtime.exit, None);
}

#[test]
fn recorded_loss_evidence_survives_a_rewrite() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = fixture_store(root.path(), SCHEMA_14);
    store.migrate_to_current().expect("migrate previous schema");

    let mut record = store.load_sessions().expect("load the session").remove(0);
    let runtime = record.info.runtime.as_mut().expect("runtime");
    runtime.loss_cause = Some("user_manager_lost".to_owned());
    store.record_session(&record).expect("rewrite the record");

    let reloaded = store.load_sessions().expect("reload the session");
    let runtime = reloaded[0].info.runtime.as_ref().expect("runtime");
    assert_eq!(runtime.loss_cause.as_deref(), Some("user_manager_lost"));
}
