//! A session record written by a newer daemon with a warning kind this build
//! does not know, loaded through the public daemon store boundary.

// Rust guideline compliant 2026-10-14

use pohunek_daemon::store::{SchemaMigration, Store};
use protocol::SessionWarningKind;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;

const SCHEMA_8: &str = include_str!("../src/store/fixtures/schema-8/metadata.jsonl");

/// Session the schema-8 daemon recorded in the fixture.
const FIXTURE_SESSION: &str = "s-01M4TRMBMMYEJYVJWER5365RPV";

#[test]
fn a_stored_session_with_an_unknown_warning_kind_loads_without_that_warning() {
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
    line["info"]["warnings"] = serde_json::json!([
        {"kind": "kind_of_a_newer_daemon", "message": "unknown to this build"},
        {"kind": "hook", "message": "hook failed"},
    ]);
    std::fs::write(&path, format!("{line}\n")).expect("rewrite the store line");

    let sessions = store.load_sessions().expect("load the session");
    assert_eq!(sessions.len(), 1, "the session is kept");
    let session = &sessions[0];
    assert_eq!(session.session_id, FIXTURE_SESSION);
    let kinds: Vec<_> = session
        .info
        .warnings
        .iter()
        .map(|warning| warning.kind)
        .collect();
    assert_eq!(kinds, vec![SessionWarningKind::Hook]);
}
