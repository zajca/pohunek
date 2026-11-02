//! Schema-13 store migration through the public daemon store boundary, and the
//! create-request record of the current schema.
//!
//! The schema-13 fixture is a record of a lost Pi session in the shape the
//! schema-13 daemon writes, with neutral paths and identifiers. A session that
//! records no create request reads as having none. The schema-15 fixture carries
//! a request whose fingerprint was computed outside the daemon.

// Rust guideline compliant 2026-11-01

use pohunek_daemon::store::{CreateRequest, SchemaMigration, Store, STORE_SCHEMA_VERSION};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;

const SCHEMA_13: &str = include_str!("../src/store/fixtures/schema-13/metadata.jsonl");
const SCHEMA_15: &str = include_str!("../src/store/fixtures/schema-15/metadata.jsonl");

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
fn schema_13_sessions_migrate_without_inventing_a_create_request() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = fixture_store(root.path(), SCHEMA_13);

    let migrated = store.migrate_to_current().expect("migrate previous schema");
    let SchemaMigration::Migrated {
        from,
        to,
        records,
        backup,
    } = migrated
    else {
        panic!("schema-13 store must migrate");
    };
    assert_eq!((from, to, records), (13, STORE_SCHEMA_VERSION, 1));
    assert_eq!(
        std::fs::read_to_string(backup).expect("read backup"),
        SCHEMA_13
    );

    let sessions = store.load_sessions().expect("load migrated session");
    assert_eq!(sessions[0].create_request, None);
}

#[test]
fn a_recorded_create_request_loads_and_survives_a_rewrite() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let store = fixture_store(root.path(), SCHEMA_15);

    let mut record = store.load_sessions().expect("load the session").remove(0);
    let request = CreateRequest {
        id: "req-frozen-1".to_owned(),
        fingerprint: "v1:ddd159c0a069a9dffcd7548a926edb3e8789721705ec126c38ce4ecb01b8cabd"
            .to_owned(),
    };
    assert_eq!(record.create_request.as_deref(), Some(&request));

    record.info.name = Some("renamed".to_owned());
    store.record_session(&record).expect("rewrite the record");
    let reloaded = store.load_sessions().expect("reload the session");
    assert_eq!(reloaded[0].create_request.as_deref(), Some(&request));
    assert_eq!(reloaded[0].info.name.as_deref(), Some("renamed"));
}
