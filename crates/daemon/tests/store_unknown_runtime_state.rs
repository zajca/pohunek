//! A session record a newer daemon wrote with a runtime state this build does
//! not know, read after a rollback through the public daemon store boundary.
//! The decode stays strict, so the line is refused and left on disk.

// Rust guideline compliant 2026-10-29

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;

use pohunek_daemon::store::{SchemaMigration, Store};

const SCHEMA_10: &str = include_str!("../src/store/fixtures/schema-10/metadata.jsonl");

#[test]
fn a_stored_session_with_an_unknown_runtime_state_is_rejected_and_kept_on_disk() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let path = root.path().join("metadata.jsonl");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("create owner-private schema-10 store")
        .write_all(SCHEMA_10.as_bytes())
        .expect("write frozen schema-10 store");
    let store = Store::new(path.clone());
    assert!(matches!(
        store.migrate_to_current().expect("migrate previous schema"),
        SchemaMigration::Migrated { .. }
    ));
    assert_eq!(store.load_sessions().expect("load sessions").len(), 1);

    let stored = std::fs::read_to_string(&path).expect("read the store");
    let mut line: serde_json::Value =
        serde_json::from_str(stored.trim_end()).expect("one stored JSON line");
    assert_eq!(line["info"]["runtime"]["state"], "lost", "{line}");
    line["info"]["runtime"]["state"] = "a_state_from_a_newer_daemon".into();
    let rewritten = format!(
        "{}\n",
        serde_json::to_string(&line).expect("serialize line")
    );
    std::fs::write(&path, &rewritten).expect("rewrite the store");

    assert!(
        store.load_sessions().expect("load sessions").is_empty(),
        "the store decode stays strict"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("read the store again"),
        rewritten,
        "the rejected line stays on disk for the daemon that wrote it"
    );
}
