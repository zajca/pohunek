//! A session record whose snapshot carries an activity this build does not know
//! (written by a newer daemon, then read after a rollback), loaded through the
//! public daemon store boundary.

// Rust guideline compliant 2026-10-20

use pohunek_daemon::store::{SchemaMigration, Store};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;

const SCHEMA_8: &str = include_str!("../src/store/fixtures/schema-8/metadata.jsonl");

#[test]
fn a_stored_session_with_an_unknown_activity_is_rejected_and_kept_on_disk() {
    let root = pohunek_test_support::tempdir().expect("private fixture root");
    let path = root.path().join("metadata.jsonl");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("create owner-private schema-8 store")
        .write_all(SCHEMA_8.as_bytes())
        .expect("write frozen schema-8 store");
    let store = Store::new(path.clone());
    assert!(matches!(
        store.migrate_to_current().expect("migrate previous schema"),
        SchemaMigration::Migrated { .. }
    ));

    let stored = std::fs::read_to_string(&path).expect("read current store");
    let mut line: serde_json::Value =
        serde_json::from_str(stored.trim_end()).expect("one stored JSON line");
    line["info"]["activity"] = serde_json::json!("an_activity_from_a_newer_daemon");
    let rewritten = format!("{line}\n");
    std::fs::write(&path, &rewritten).expect("rewrite the store line");

    let sessions = store.load_sessions().expect("load the store");
    assert!(
        sessions.is_empty(),
        "the store decode stays strict: {} session(s) loaded",
        sessions.len()
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("read the store again"),
        rewritten,
        "the rejected line stays on disk for the daemon that wrote it"
    );
}
