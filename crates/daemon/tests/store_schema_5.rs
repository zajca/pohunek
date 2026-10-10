//! Schema-4 store migration through the public daemon store boundary.

// Rust guideline compliant 2026-10-09

use pohunek_daemon::store::{SchemaMigration, Store, STORE_SCHEMA_VERSION};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;

const SCHEMA_4: &str = include_str!("../src/store/fixtures/schema-4/metadata.jsonl");

#[test]
fn schema_4_session_migrates_without_inventing_native_activity() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let path = root.path().join("metadata.jsonl");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("create owner-private schema-4 store")
        .write_all(SCHEMA_4.as_bytes())
        .expect("write frozen schema-4 store");
    let store = Store::new(path.clone());

    let migrated = store.migrate_to_current().expect("migrate previous schema");
    let SchemaMigration::Migrated {
        from,
        to,
        records,
        backup,
    } = migrated
    else {
        panic!("schema-4 store must migrate");
    };
    assert_eq!((from, to, records), (4, STORE_SCHEMA_VERSION, 1));
    assert_eq!(
        std::fs::read_to_string(backup).expect("read backup"),
        SCHEMA_4
    );

    let stored = std::fs::read_to_string(&path).expect("read migrated store");
    let line: serde_json::Value = serde_json::from_str(stored.trim()).expect("stored JSON");
    assert_eq!(line["schema_version"], STORE_SCHEMA_VERSION);
    assert!(
        line["info"].get("native_last_activity_at").is_none(),
        "transcript activity must be computed from the file, not migrated from metadata"
    );
    let sessions = store.load_sessions().expect("load migrated session");
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0].info.native_session_id.as_deref(),
        Some("fixture-conversation")
    );
    assert_eq!(sessions[0].info.native_last_activity_at, None);
}
