//! Schema-18 store migration through the public daemon store boundary, and the
//! end time of a finished session in the current schema.
//!
//! The schema-18 fixture is a stopped session in the shape the schema-18 daemon
//! writes: no end time, and a last update the retention clock had to use. The
//! migration copies that update into `ended_at`, and an `ended_at` already
//! recorded survives a rewrite of the record.

// Rust guideline compliant 2026-11-11

use pohunek_daemon::store::{SchemaMigration, Store, STORE_SCHEMA_VERSION};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;

const SCHEMA_18_FINISHED: &str = include_str!("../src/store/fixtures/schema-18/finished.jsonl");
const SCHEMA_18_LOST: &str = include_str!("../src/store/fixtures/schema-18/metadata.jsonl");
const SCHEMA_19: &str = include_str!("../src/store/fixtures/schema-19/metadata.jsonl");

/// The last update of the schema-18 finished fixture.
const FINISHED_UPDATED_AT: &str = "2026-10-13T13:53:30.947477438Z";

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
fn schema_18_finished_session_gets_its_last_update_as_end_time() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = fixture_store(root.path(), SCHEMA_18_FINISHED);

    let migrated = store.migrate_to_current().expect("migrate previous schema");
    let SchemaMigration::Migrated {
        from,
        to,
        records,
        backup,
    } = migrated
    else {
        panic!("schema-18 store must migrate");
    };
    assert_eq!((from, to, records), (18, STORE_SCHEMA_VERSION, 1));
    assert_eq!(
        std::fs::read_to_string(backup).expect("read backup"),
        SCHEMA_18_FINISHED
    );

    let session = store
        .load_sessions()
        .expect("load migrated sessions")
        .remove(0);
    assert_eq!(session.info.updated_at, FINISHED_UPDATED_AT);
    assert_eq!(session.info.ended_at.as_deref(), Some(FINISHED_UPDATED_AT));
}

#[test]
fn schema_18_failed_lost_session_ends_at_its_last_update() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = fixture_store(root.path(), SCHEMA_18_LOST);

    store.migrate_to_current().expect("migrate previous schema");

    let session = store
        .load_sessions()
        .expect("load migrated sessions")
        .remove(0);
    assert_eq!(
        session.info.ended_at.as_deref(),
        Some(session.info.updated_at.as_str()),
        "a failed session is finished whatever its runtime"
    );
}

#[test]
fn a_recorded_end_time_loads_and_survives_a_rewrite() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = fixture_store(root.path(), SCHEMA_19);
    let ended = Some("2026-10-14T08:00:00.123456789Z".to_owned());

    let mut record = store.load_sessions().expect("load the session").remove(0);
    assert_eq!(record.info.ended_at, ended);
    record.info.name = Some("renamed".to_owned());
    "2026-11-03T00:00:00Z".clone_into(&mut record.info.updated_at);
    store.record_session(&record).expect("rewrite the record");

    let reloaded = store.load_sessions().expect("reload the session");
    assert_eq!(reloaded[0].info.ended_at, ended);
}
